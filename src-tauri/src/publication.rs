//! The desktop's publication seam: which revision the viewer is showing, and which it must fetch.
//!
//! [`crate::document::AppState`] owns the authoritative revisions; the window owns the picture.
//! This module is the bridge between the two:
//!
//! - **every commit** records the new revision on a [`splatmcp_core::PublicationTracker`],
//!   including a commit nobody asked to display, so a hidden revision is reported as committed
//!   and *not* displayed rather than as the current one;
//! - **every publication** mints a request token before the event is emitted, and the token is
//!   what the acknowledgement must quote;
//! - **every pending request expires** after [`ACK_TIMEOUT_MS`]: the tracker sweeps on read and
//!   on the next publication, so a viewer that stopped answering is reported as `timed_out`
//!   instead of leaving a caller waiting on a promise that will never settle;
//! - **the binary payload** the viewer fetches is addressed by `(document_id, revision)`, so a
//!   delayed old fetch cannot be mistaken for a newer one.
//!
//! The tracker holds no geometry and takes no document lock: it answers "what is on screen?"
//! without touching the store, which keeps a status query from waiting on a serialisation.

use std::sync::Arc;

use serde_json::json;
use tauri::{Emitter, Manager};
#[cfg(test)]
use splatmcp_core::PublicationOutcome;
use splatmcp_core::{
    document::now_ms, DocumentHandle, PublicationError, PublicationNoticeOutcome,
    PublicationRequest, PublicationSource, PublicationStatus, PublicationTracker,
    RendererCapabilities, SideEffectState, ACK_TIMEOUT_MS,
};

/// The process-wide publication tracker.
pub struct PublicationHost {
    tracker: Arc<PublicationTracker>,
    /// How long a publication waits for the viewer's acknowledgement.
    ack_timeout_ms: u64,
    /// Reads the clock, so a test can drive expiry without sleeping.
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl Default for PublicationHost {
    fn default() -> Self {
        Self::new(ACK_TIMEOUT_MS)
    }
}

impl PublicationHost {
    /// A host whose publications time out after `ack_timeout_ms`.
    pub fn new(ack_timeout_ms: u64) -> Self {
        Self::with_clock(ack_timeout_ms, Arc::new(now_ms))
    }

    /// The same host, reading time from `clock`. Used by tests that drive the timeout.
    pub fn with_clock(
        ack_timeout_ms: u64,
        clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    ) -> Self {
        Self {
            tracker: Arc::new(PublicationTracker::new()),
            ack_timeout_ms,
            clock,
        }
    }

    /// The tracker itself, for a caller that has typed work to do.
    pub fn tracker(&self) -> &Arc<PublicationTracker> {
        &self.tracker
    }

    fn now(&self) -> u64 {
        (self.clock)()
    }

    /// Records that the store holds this revision.
    ///
    /// Called from every commit path, displayed or not: that is what makes a hidden revision
    /// show up as "committed, not displayed" instead of leaving the status on the old one.
    pub fn committed(&self, handle: &DocumentHandle) -> Result<(), PublicationError> {
        self.tracker
            .committed(handle.document_id.as_str(), handle.revision)
    }

    /// Starts a publication and returns the request the viewer must fetch.
    pub fn begin(
        &self,
        handle: &DocumentHandle,
        source: PublicationSource,
        frame: bool,
    ) -> Result<PublicationRequest, PublicationError> {
        // The commit that led here is recorded first, so a publication can never announce a
        // revision the tracker does not know is committed.
        self.committed(handle)?;
        self.tracker.begin_at(
            self.now(),
            self.ack_timeout_ms,
            handle.document_id.as_str(),
            handle.revision,
            source,
            frame,
        )
    }

    /// Records a viewer acknowledgement and returns the resulting status.
    pub fn acknowledge(
        &self,
        document_id: &str,
        revision: u64,
        token: u64,
    ) -> Result<PublicationStatus, PublicationError> {
        self.tracker.acknowledge_at(
            self.now(),
            self.ack_timeout_ms,
            document_id,
            revision,
            token,
        )
    }

    /// Records a viewer failure: the previous model stays displayed.
    pub fn fail(
        &self,
        document_id: &str,
        revision: u64,
        reason: impl Into<String>,
    ) -> Result<PublicationStatus, PublicationError> {
        self.tracker
            .fail_at(self.now(), self.ack_timeout_ms, document_id, revision, reason)
    }

    /// Marks the request in flight as timed out, for a caller that is giving up on this viewer.
    ///
    /// The automatic case is the sweep inside the tracker; this is the explicit one, exercised by
    /// a test that has already stopped waiting.
    #[cfg(test)]
    pub fn time_out(
        &self,
        document_id: &str,
    ) -> Result<Option<PublicationRequest>, PublicationError> {
        self.tracker.time_out(document_id)
    }

    /// One document's publication status, if anything is known about it.
    ///
    /// Reading it also applies the acknowledgement timeout, so a stalled publication is
    /// reported as timed out rather than as pending forever.
    pub fn status(
        &self,
        document_id: &str,
    ) -> Result<Option<PublicationStatus>, PublicationError> {
        self.tracker
            .status_at(self.now(), self.ack_timeout_ms, document_id)
    }

    /// The document that currently owns the screen, if any.
    pub fn active_document(&self) -> Result<Option<String>, PublicationError> {
        self.tracker.active_document()
    }

    /// Renderer capabilities, with the timeout this app actually uses.
    ///
    /// The displayed revision comes from the tracker when it has one, because that is the value
    /// the app itself acknowledges; the viewer's own report is used for what only the renderer
    /// knows (whether a splat is loaded, and its gaussian count).
    pub fn capabilities(
        &self,
        displayed_elsewhere: Option<u64>,
        displayed_point_count: usize,
    ) -> RendererCapabilities {
        RendererCapabilities::of(displayed_elsewhere, displayed_point_count, self.ack_timeout_ms)
    }
}

/// Managed state wrapper so the bridge, the commands and the job adapters share one tracker.
pub struct PublicationHostState(pub Arc<PublicationHost>);

/// The payload of the event that asks the viewer to display one exact request.
///
/// An event, never a payload carrier: identity, revision, token and the two switches. The geometry
/// travels through the binary `splat_bytes_for_revision` response. The app builds this shape in
/// `announce`; the helper exists so a test can assert the contract.
#[cfg(test)]
pub fn request_payload(request: &PublicationRequest, file_name: &str, point_count: usize) -> serde_json::Value {
    json!({
        "contract_version": splatmcp_core::PUBLICATION_CONTRACT_VERSION,
        "document_id": request.document_id,
        "revision": request.revision,
        "token": request.token,
        "source": request.source.as_str(),
        "frame": request.frame,
        "file_name": file_name,
        "point_count": point_count,
    })
}

/// True for the outcome that means the requested revision did *not* reach the screen.
#[cfg(test)]
pub fn needs_attention(outcome: &PublicationOutcome) -> bool {
    matches!(
        outcome,
        PublicationOutcome::Failed(_)
            | PublicationOutcome::TimedOut
            | PublicationOutcome::Skipped { .. }
    )
}

/// A publication status as the window and a tool read it.
pub fn status_json(status: &PublicationStatus) -> serde_json::Value {
    json!({
        "contract_version": status.contract_version,
        "document_id": status.document_id,
        "committed_revision": status.committed_revision,
        "displayed_revision": status.displayed_revision,
        "is_current": status.is_current(),
        "display_lagging": status.display_lagging,
        "displayed_elsewhere": status.displayed_elsewhere,
        "pending": status.pending.as_ref().map(|request| json!({
            "revision": request.revision,
            "token": request.token,
            "source": request.source.as_str(),
            "frame": request.frame,
            "started_at_ms": request.started_at_ms,
        })),
        "last": status.last.as_ref().map(|(request, outcome)| json!({
            "revision": request.revision,
            "token": request.token,
            "outcome": outcome.as_str(),
            "detail": outcome.to_string(),
        })),
        "skipped": status.skipped,
        "failures": status.failures.iter().map(|(revision, reason)| json!({
            "revision": revision,
            "reason": reason,
        })).collect::<Vec<_>>(),
        "summary": status.summary(),
    })
}

/// Renderer capabilities as the window and a tool read them.
pub fn capabilities_json(capabilities: &RendererCapabilities) -> serde_json::Value {
    json!({
        "viewer_ready": capabilities.viewer_ready,
        "has_splat": capabilities.has_splat,
        "displayed_revision": capabilities.displayed_revision,
        "displayed_point_count": capabilities.displayed_point_count,
        "transport": capabilities.transport,
        "revision_addressed": capabilities.revision_addressed,
        "ack_timeout_ms": capabilities.ack_timeout_ms,
        "summary": capabilities.summary(),
    })
}

/// A publication failure as a structured reply: code plus sentence.
pub fn error_json(error: &PublicationError) -> serde_json::Value {
    json!({ "code": error.code(), "message": error.to_string() })
}

/// Emits the publication event for one exact revision and records the request.
///
/// One implementation for every producer - a load, an edit batch, a preview commit, a Python
/// job - so the viewer sees the same request shape and the tracker sees the same bookkeeping.
/// The commit is recorded first, then a token is minted, then the identity-only event is sent;
/// the geometry travels separately through the binary `(document, revision)` response.
///
/// An error means the request was recorded but could not be announced, and the publication is
/// marked failed so the status does not leave it looking pending.
pub fn announce(
    app: &tauri::AppHandle,
    receipt: &splatmcp_core::ReceiptDocument,
    component_id: Option<String>,
    frame: bool,
) -> Result<PublicationRequest, PublicationError> {
    let publications = app.state::<PublicationHostState>().0.clone();
    let handle = receipt.handle();
    let request = publications.begin(&handle, PublicationSource::Committed, frame)?;
    let payload = crate::python::RevisionPayload {
        revision: receipt.revision,
        document_id: receipt.document_id.to_string(),
        token: request.token,
        source: request.source.as_str().to_owned(),
        file_name: receipt.file_name.clone(),
        point_count: receipt.point_count,
        component_id,
        frame,
    };
    if let Err(error) = app.emit_to(crate::viewer::VIEWER_WINDOW, crate::bridge::EDIT_REVISION_EVENT, payload)
    {
        let _ = publications.fail(
            request.document_id.as_str(),
            request.revision,
            format!("could not tell the viewer: {error}"),
        );
        return Err(PublicationError::Unavailable {
            reason: format!("could not tell the viewer about {handle}: {error}"),
        });
    }
    Ok(request)
}

/// The viewer status type this module reads, named once.
type ViewerStatus = splatmcp_bridge::ViewerStatus;

/// Applies the publication outcomes the tracker has produced to the records that promised them.
///
/// A job stores `display: pending` when it announces its revision; the acknowledgement, the
/// timeout, the failure or the supersede arrives later and is recorded here, so a job's display
/// field converges instead of freezing at submission time. Called from every read that can observe
/// an outcome - a status query, a job query, an acknowledgement - so a caller always sees the
/// current answer after asking once.
///
/// Returns how many job receipts were updated.
pub fn apply_publication_notices(app: &tauri::AppHandle) -> usize {
    let publications = app.state::<PublicationHostState>().0.clone();
    let notices = match publications.tracker().take_notices() {
        Ok(notices) => notices,
        Err(_) => return 0,
    };
    if notices.is_empty() {
        return 0;
    }
    let jobs = app.state::<crate::jobs::JobHostState>().0.clone();
    let mut updated = 0;
    for notice in &notices {
        // A job is matched by the revision it produced, so a notice for another revision or
        // another document touches nothing.
        let Some(document_id) = splatmcp_core::DocumentId::parse(&notice.document_id) else {
            continue;
        };
        let handle = DocumentHandle::new(document_id, notice.revision);
        updated += jobs.service().note_display(&handle, effect_of(&notice.outcome));
    }
    updated
}

/// The job display state that matches a publication outcome.
fn effect_of(outcome: &PublicationNoticeOutcome) -> SideEffectState {
    match outcome {
        PublicationNoticeOutcome::Displayed => SideEffectState::Done,
        PublicationNoticeOutcome::Failed(reason) => SideEffectState::Failed(reason.clone()),
        PublicationNoticeOutcome::TimedOut => SideEffectState::TimedOut,
        PublicationNoticeOutcome::Superseded => SideEffectState::Superseded,
    }
}

/// What the renderer is doing, assembled from the app's own records.
///
/// The displayed revision comes from the tracker (the app acknowledged it) and the point count from
/// the store's metadata for that exact revision, because those are the values `publication_status`
/// reports. The viewer supplies only what only a renderer knows - whether it answered, and whether
/// a splat is loaded - so the two replies can never contradict each other about identity.
pub fn capabilities_for(app: &tauri::AppHandle) -> serde_json::Value {
    let publications = app.state::<PublicationHostState>().0.clone();
    apply_publication_notices(app);
    let state = app.state::<crate::document::AppState>();
    let handle = state.active_handle();
    let displayed_revision = handle
        .as_ref()
        .and_then(|handle| {
            publications
                .status(handle.document_id.as_str())
                .ok()
                .flatten()
                .and_then(|status| status.displayed_revision)
        });
    // The count of the revision that is actually displayed, read from the store rather than from
    // the renderer: a viewer that has not reloaded yet would otherwise report the previous model's
    // count next to the new revision.
    let displayed_point_count = match (&handle, displayed_revision) {
        (Some(handle), Some(revision)) => state
            .snapshot(splatmcp_core::Expected::Handle(DocumentHandle::new(
                handle.document_id.clone(),
                revision,
            )))
            .map(|snapshot| snapshot.metadata().point_count)
            .unwrap_or(0),
        _ => 0,
    };
    let viewer_status = app
        .state::<crate::bridge::ViewerState>()
        .0
        .request(
            splatmcp_bridge::Method::ViewerStatus,
            serde_json::Value::Null,
            crate::viewer::VIEWER_TIMEOUT,
        )
        .ok()
        .and_then(|value| serde_json::from_value::<ViewerStatus>(value).ok());
    let mut capabilities = publications.capabilities(displayed_revision, displayed_point_count);
    if let Some(status) = &viewer_status {
        capabilities.viewer_ready = status.viewer_ready;
        capabilities.has_splat = status.loaded;
    }
    capabilities_json(&capabilities)
}

/// Tauri command: the status of one document's publication, or of every tracked document.
#[tauri::command]
pub fn publication_status(
    app: tauri::AppHandle,
    host: tauri::State<'_, PublicationHostState>,
    document_id: Option<String>,
) -> Result<serde_json::Value, String> {
    // Reading a status is also where an expired publication is noticed and applied, so the jobs
    // that announced a display see the timeout without a second call.
    apply_publication_notices(&app);
    match document_id {
        Some(document_id) => match host.0.status(&document_id) {
            Ok(Some(status)) => Ok(status_json(&status)),
            Ok(None) => Ok(json!({
                "document_id": document_id,
                "note": "no publication has been started for this document in this session"
            })),
            Err(error) => Ok(json!({ "error": error_json(&error) })),
        },
        None => {
            let statuses = host
                .0
                .tracker()
                .all()
                .map_err(|error| error.to_string())?;
            Ok(json!({
                "documents": statuses.iter().map(status_json).collect::<Vec<_>>(),
                "active_document": host.0.active_document().map_err(|error| error.to_string())?,
            }))
        }
    }
}

/// Tauri command: renderer capabilities, assembled from the app's own records.
#[tauri::command]
pub fn renderer_capabilities(app: tauri::AppHandle) -> serde_json::Value {
    capabilities_for(&app)
}

/// Tauri command: the binary payload of one exact revision of one exact document.
///
/// This is the seam that replaces base64 PLY in events: the viewer asks for `(document,
/// revision)` and receives bytes, so nothing large travels as JSON. The revision is resolved
/// against the store, and an unknown or expired revision is an error the viewer reports as a
/// display failure instead of drawing something else.
#[tauri::command]
pub fn splat_bytes_for_handle(
    state: tauri::State<'_, crate::document::AppState>,
    document_id: String,
    revision: u64,
) -> Result<tauri::ipc::Response, String> {
    let handle = crate::document::handle_of(&document_id, revision)?;
    // The bytes are serialised from an immutable snapshot, so the store is not held while a
    // large revision is written out, and the revision cannot change under the viewer.
    let (_, bytes) = state.ply_bytes_for(&handle)?;
    Ok(tauri::ipc::Response::new(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use splatmcp_core::{DocumentId, PublicationNoticeOutcome, PublicationTracker,
                        PUBLICATION_CONTRACT_VERSION};
    use std::sync::atomic::{AtomicU64, Ordering};

    fn handle() -> DocumentHandle {
        DocumentHandle::new(DocumentId::mint(0x4f2a, 1), 4)
    }

    /// A host on a clock the test moves by hand.
    fn clocked() -> (PublicationHost, Arc<AtomicU64>) {
        let clock = Arc::new(AtomicU64::new(1_000));
        let reader = Arc::clone(&clock);
        let host = PublicationHost::with_clock(1_000, Arc::new(move || reader.load(Ordering::SeqCst)));
        (host, clock)
    }

    #[test]
    fn a_commit_is_recorded_but_never_reported_as_displayed() {
        let host = PublicationHost::new(1_000);
        host.committed(&handle()).unwrap();
        let status = host.status("doc-4f2a-1").unwrap().unwrap();
        assert_eq!(status.committed_revision, Some(4));
        assert_eq!(status.displayed_revision, None);
        assert!(status.display_lagging);
        let encoded = status_json(&status);
        assert_eq!(encoded["is_current"], false);
        assert_eq!(encoded["display_lagging"], true);
    }

    #[test]
    fn a_hidden_commit_moves_the_committed_revision_and_stops_claiming_current() {
        // The reported defect: a `display:false` edit left the status on the old revision and
        // still claimed the display was current.
        let host = PublicationHost::new(1_000);
        let request = host
            .begin(&handle(), PublicationSource::Committed, false)
            .unwrap();
        host.acknowledge("doc-4f2a-1", 4, request.token).unwrap();

        // A hidden commit of revision 5: recorded, not published.
        let hidden = DocumentHandle::new(DocumentId::mint(0x4f2a, 1), 5);
        host.committed(&hidden).unwrap();
        let status = host.status("doc-4f2a-1").unwrap().unwrap();
        assert_eq!(status.committed_revision, Some(5));
        assert_eq!(status.displayed_revision, Some(4));
        assert!(!status.is_current());
        assert!(status.display_lagging);
        assert!(status.pending.is_none(), "a hidden commit publishes nothing");
        let encoded = status_json(&status);
        assert_eq!(encoded["is_current"], false);
        assert_eq!(encoded["committed_revision"], 5);
        assert_eq!(encoded["displayed_revision"], 4);
    }

    #[test]
    fn a_pending_publication_expires_instead_of_waiting_forever() {
        let (host, clock) = clocked();
        let request = host
            .begin(&handle(), PublicationSource::Committed, false)
            .unwrap();
        // Still inside the timeout: the request is honestly pending.
        clock.store(1_500, Ordering::SeqCst);
        let status = host.status("doc-4f2a-1").unwrap().unwrap();
        assert_eq!(status.pending.as_ref().map(|p| p.token), Some(request.token));
        assert_eq!(status.pending.as_ref().map(|p| p.started_at_ms), Some(1_000));

        // Past it: the read applies the timeout, so a stall is reported as a stall.
        clock.store(1_000 + ACK_TIMEOUT_MS + 1, Ordering::SeqCst);
        let status = host.status("doc-4f2a-1").unwrap().unwrap();
        assert!(status.pending.is_none());
        assert_eq!(
            status.last.as_ref().map(|(_, outcome)| outcome.clone()),
            Some(PublicationOutcome::TimedOut)
        );
        assert!(needs_attention(&PublicationOutcome::TimedOut));
    }

    #[test]
    fn a_new_publication_expires_the_stalled_one_it_supersedes() {
        let (host, clock) = clocked();
        let first = host
            .begin(&handle(), PublicationSource::Committed, false)
            .unwrap();
        clock.store(1_000 + ACK_TIMEOUT_MS + 5, Ordering::SeqCst);
        // The next publication sweeps first, so the stalled request is timed out before the
        // new one is queued: it is never left looking like work still in progress.
        let second = host
            .begin(&handle(), PublicationSource::Committed, false)
            .unwrap();
        assert_ne!(first.token, second.token);
        let status = host.status("doc-4f2a-1").unwrap().unwrap();
        assert_eq!(status.pending.as_ref().map(|p| p.token), Some(second.token));
    }

    #[test]
    fn a_publication_round_trips_through_its_token() {
        let host = PublicationHost::new(1_000);
        let request = host
            .begin(&handle(), PublicationSource::Committed, false)
            .unwrap();
        assert_eq!(request.revision, 4);
        assert_eq!(request.token, 1);
        let payload = request_payload(&request, "scene.ply", 12);
        assert_eq!(payload["token"], 1);
        assert_eq!(payload["revision"], 4);
        assert_eq!(payload["point_count"], 12);
        assert_eq!(payload["contract_version"], PUBLICATION_CONTRACT_VERSION);
        // Nothing large travels in the event: only identity and switches.
        assert!(payload.to_string().len() < 260, "{payload}");

        let status = host.acknowledge("doc-4f2a-1", 4, request.token).unwrap();
        assert!(status.is_current());
        assert_eq!(status.displayed_revision, Some(4));
        let encoded = status_json(&status);
        assert_eq!(encoded["last"]["outcome"], "displayed");
    }

    #[test]
    fn a_stale_acknowledgement_is_refused_and_the_newest_keeps_the_screen() {
        let (host, clock) = clocked();
        let four = handle();
        let five = DocumentHandle::new(DocumentId::mint(0x4f2a, 1), 5);
        let first = host
            .begin(&four, PublicationSource::Committed, true)
            .unwrap();
        clock.store(1_100, Ordering::SeqCst);
        let second = host
            .begin(&five, PublicationSource::Committed, false)
            .unwrap();
        assert_ne!(first.token, second.token);
        // The delayed acknowledgement of revision 4 arrives after 5 was announced.
        let error = host.acknowledge("doc-4f2a-1", 4, first.token).unwrap_err();
        assert_eq!(error.code(), "stale_acknowledgement");
        let status = host.acknowledge("doc-4f2a-1", 5, second.token).unwrap();
        assert_eq!(status.displayed_revision, Some(5));
        assert!(status.skipped.contains(&4));
    }

    #[test]
    fn a_failed_publication_keeps_the_old_frame_and_reports_the_failure() {
        let (host, clock) = clocked();
        let shown = host
            .begin(&handle(), PublicationSource::Committed, true)
            .unwrap();
        host.acknowledge("doc-4f2a-1", 4, shown.token).unwrap();
        clock.store(2_000, Ordering::SeqCst);
        let hidden = DocumentHandle::new(DocumentId::mint(0x4f2a, 1), 5);
        host.committed(&hidden).unwrap();
        host.begin(&hidden, PublicationSource::Committed, false).unwrap();
        let status = host.fail("doc-4f2a-1", 5, "parse failed").unwrap();
        assert_eq!(status.displayed_revision, Some(4), "the old frame stays");
        assert_eq!(status.committed_revision, Some(5));
        let encoded = status_json(&status);
        assert_eq!(encoded["failures"][0]["revision"], 5);
        assert_eq!(encoded["failures"][0]["reason"], "parse failed");
        assert!(encoded["summary"].as_str().unwrap().contains("last request failed"));
    }

    #[test]
    fn switching_documents_moves_the_screen_and_the_old_one_stops_claiming_it() {
        // The reported stall: the status of the new document had to say "not displayed yet",
        // and the old document had to stop reporting itself as displayed.
        let host = PublicationHost::new(1_000);
        let a = DocumentHandle::new(DocumentId::mint(0x4f2a, 1), 7);
        let shown = host
            .begin(&a, PublicationSource::Committed, true)
            .unwrap();
        host.acknowledge(a.document_id.as_str(), 7, shown.token)
            .unwrap();
        assert_eq!(host.active_document().unwrap().as_deref(), Some("doc-4f2a-1"));

        // Document B becomes the displayed one.
        let b = DocumentHandle::new(DocumentId::mint(0x4f2a, 2), 1);
        let first_b = host.begin(&b, PublicationSource::Committed, true).unwrap();
        let status_b = host
            .acknowledge(b.document_id.as_str(), 1, first_b.token)
            .unwrap();
        assert_eq!(status_b.displayed_revision, Some(1));
        assert_eq!(host.active_document().unwrap().as_deref(), Some("doc-4f2a-2"));

        let status_a = host.status(a.document_id.as_str()).unwrap().unwrap();
        assert_eq!(status_a.displayed_revision, None, "A is not on screen any more");
        assert_eq!(status_a.committed_revision, Some(7));
        assert!(status_a.display_lagging);
    }

    #[test]
    fn a_document_that_has_never_been_displayed_says_so_while_another_one_is() {
        let host = PublicationHost::new(1_000);
        let a = DocumentHandle::new(DocumentId::mint(0x4f2a, 1), 1);
        let shown = host.begin(&a, PublicationSource::Committed, true).unwrap();
        host.acknowledge(a.document_id.as_str(), 1, shown.token)
            .unwrap();

        // B has a commit but no publication yet: the status must distinguish that from "B is
        // displayed somewhere".
        let b = DocumentHandle::new(DocumentId::mint(0x4f2a, 2), 3);
        host.committed(&b).unwrap();
        let status = host.status(b.document_id.as_str()).unwrap().unwrap();
        assert_eq!(status.displayed_revision, None);
        assert!(status.displayed_elsewhere, "A owns the screen");
        let encoded = status_json(&status);
        assert_eq!(encoded["displayed_elsewhere"], true);
        assert!(encoded["summary"].as_str().unwrap().contains("another document is displayed"));
    }

    #[test]
    fn capabilities_name_the_transport_and_the_timeout_this_app_uses() {
        let host = PublicationHost::new(1_500);
        let capabilities = host.capabilities(Some(7), 200_000);
        let encoded = capabilities_json(&capabilities);
        assert_eq!(encoded["transport"], "tauri_binary_response");
        assert_eq!(encoded["revision_addressed"], true);
        assert_eq!(encoded["ack_timeout_ms"], 1_500);
        assert_eq!(encoded["displayed_revision"], 7);
        assert!(encoded["summary"].as_str().unwrap().contains("200000 gaussians"));
    }

    #[test]
    fn a_timeout_names_the_request_that_never_answered() {
        let host = PublicationHost::new(10);
        let request = host
            .begin(&handle(), PublicationSource::Committed, false)
            .unwrap();
        let timed_out = host.time_out("doc-4f2a-1").unwrap().unwrap();
        assert_eq!(timed_out.token, request.token);
        let status = host.status("doc-4f2a-1").unwrap().unwrap();
        assert_eq!(
            status.last.as_ref().map(|(_, outcome)| outcome.clone()),
            Some(PublicationOutcome::TimedOut)
        );
        assert!(status.pending.is_none());
    }

    #[test]
    fn a_publication_notice_maps_onto_the_job_display_state() {
        // The mapping is what a job's `display` field reports, so each outcome has to land on its
        // own state rather than on a generic "failed".
        assert_eq!(
            effect_of(&PublicationNoticeOutcome::Displayed),
            SideEffectState::Done
        );
        assert_eq!(
            effect_of(&PublicationNoticeOutcome::TimedOut),
            SideEffectState::TimedOut
        );
        assert_eq!(
            effect_of(&PublicationNoticeOutcome::Superseded),
            SideEffectState::Superseded
        );
        assert_eq!(
            effect_of(&PublicationNoticeOutcome::Failed("parse failed".to_owned())),
            SideEffectState::Failed("parse failed".to_owned())
        );
        // Every state names itself, which is what the wire reply carries.
        assert_eq!(SideEffectState::TimedOut.as_str(), "timed_out");
        assert_eq!(SideEffectState::Superseded.as_str(), "superseded");
        assert!(SideEffectState::Pending.is_pending());
        assert!(!SideEffectState::Done.is_pending());
    }

    #[test]
    fn the_tracker_reports_an_acknowledgement_and_a_timeout_as_notices_for_the_app() {
        // The desktop's path: the tracker produces notices, and they name the revision the app
        // stored on the job it is going to update.
        let (host, clock) = clocked();
        let request = host
            .begin(&handle(), PublicationSource::Committed, false)
            .unwrap();
        assert!(host.tracker().take_notices().unwrap().is_empty(), "nothing yet");
        host.acknowledge("doc-4f2a-1", request.revision, request.token)
            .unwrap();
        let notices = host.tracker().take_notices().unwrap();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].document_id, "doc-4f2a-1");
        assert_eq!(notices[0].revision, 4);
        assert_eq!(notices[0].outcome, PublicationNoticeOutcome::Displayed);

        // And a timeout, observed by a read, produces its own notice. Revision 5, because
        // revision 4 is already displayed (and re-publishing it would be refused).
        clock.store(9_000, Ordering::SeqCst);
        let five = DocumentHandle::new(DocumentId::mint(0x4f2a, 1), 5);
        let slow = host
            .begin(&five, PublicationSource::Committed, false)
            .unwrap();
        let _ = host.tracker().take_notices().unwrap();
        clock.store(
            9_000 + splatmcp_core::ACK_TIMEOUT_MS + 1,
            Ordering::SeqCst,
        );
        let status = host.status("doc-4f2a-1").unwrap().unwrap();
        assert!(status.pending.is_none());
        let notices = host.tracker().take_notices().unwrap();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].revision, slow.revision);
        assert_eq!(notices[0].outcome, PublicationNoticeOutcome::TimedOut);
    }

    #[test]
    fn the_tracker_is_shared_rather_than_copied() {
        let host = PublicationHost::new(1_000);
        let tracker: Arc<PublicationTracker> = Arc::clone(host.tracker());
        tracker.committed("doc-9-1", 2).unwrap();
        assert_eq!(
            host.status("doc-9-1").unwrap().unwrap().committed_revision,
            Some(2)
        );
    }
}
