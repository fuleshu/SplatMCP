//! The desktop's publication seam: which revision the viewer is showing, and which it must fetch.
//!
//! [`crate::document::AppState`] owns the authoritative revisions; the window owns the picture.
//! This module is the bridge between the two, and it is deliberately small:
//!
//! - every commit records the new revision on a [`splatmcp_core::PublicationTracker`];
//! - every publication mints a request token *before* the event is emitted, so the viewer's
//!   acknowledgement can be matched to the exact request it answers;
//! - the binary payload the viewer fetches is addressed by `(document_id, revision)`, so a
//!   delayed old fetch cannot be mistaken for a newer one;
//! - a frame the viewer could not prepare leaves the previous model displayed and is reported
//!   as a failure, never as a displayed revision.
//!
//! The tracker holds no geometry and takes no document lock: it answers "what is on screen?"
//! without touching the store, which is what keeps a status query from waiting on a large
//! serialisation.

use std::sync::Arc;

use serde_json::json;
use tauri::Manager;
use splatmcp_core::{
    DocumentHandle, PUBLICATION_CONTRACT_VERSION, PublicationError, PublicationOutcome,
    PublicationRequest, PublicationSource, PublicationStatus, PublicationTracker,
    RendererCapabilities, RevisionRecord,
};

/// The process-wide publication tracker.
pub struct PublicationHost {
    tracker: Arc<PublicationTracker>,
    /// How long a publication waits for the viewer's acknowledgement.
    ack_timeout_ms: u64,
}

impl Default for PublicationHost {
    fn default() -> Self {
        Self::new(5_000)
    }
}

impl PublicationHost {
    /// A host whose publications time out after `ack_timeout_ms`.
    pub fn new(ack_timeout_ms: u64) -> Self {
        Self {
            tracker: Arc::new(PublicationTracker::new()),
            ack_timeout_ms,
        }
    }

    /// The tracker itself, for a caller that has typed work to do.
    pub fn tracker(&self) -> &Arc<PublicationTracker> {
        &self.tracker
    }

    /// How long a publication waits for acknowledgement.
    pub fn ack_timeout_ms(&self) -> u64 {
        self.ack_timeout_ms
    }

    /// Records that the store holds this revision.
    pub fn committed(&self, handle: &DocumentHandle) -> Result<(), PublicationError> {
        self.tracker
            .committed(handle.document_id.as_str(), handle.revision)
    }

    /// Records several commits at once, for a caller that already has the history.
    pub fn committed_all(&self, handle: &DocumentHandle, history: &[RevisionRecord]) {
        let _ = self.committed(handle);
        for record in history {
            let _ = self
                .tracker
                .committed(handle.document_id.as_str(), record.revision);
        }
        let _ = self.committed(handle);
    }

    /// Starts a publication and returns the request the viewer must fetch.
    pub fn begin(
        &self,
        handle: &DocumentHandle,
        source: PublicationSource,
        frame: bool,
    ) -> Result<PublicationRequest, PublicationError> {
        self.tracker
            .committed(handle.document_id.as_str(), handle.revision)?;
        self.tracker.begin(
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
        self.tracker.acknowledge(document_id, revision, token)
    }

    /// Records a viewer failure: the previous model stays displayed.
    pub fn fail(
        &self,
        document_id: &str,
        revision: u64,
        reason: impl Into<String>,
    ) -> Result<PublicationStatus, PublicationError> {
        self.tracker.fail(document_id, revision, reason)
    }

    /// Marks the request in flight as timed out.
    pub fn time_out(
        &self,
        document_id: &str,
    ) -> Result<Option<PublicationRequest>, PublicationError> {
        self.tracker.time_out(document_id)
    }

    /// One document's publication status, if anything is known about it.
    pub fn status(
        &self,
        document_id: &str,
    ) -> Result<Option<PublicationStatus>, PublicationError> {
        self.tracker.status(document_id)
    }

    /// Renderer capabilities, with the timeout this app actually uses.
    pub fn capabilities(
        &self,
        displayed_revision: Option<u64>,
        displayed_point_count: usize,
    ) -> RendererCapabilities {
        RendererCapabilities::of(displayed_revision, displayed_point_count, self.ack_timeout_ms)
    }
}

/// Managed state wrapper so the bridge, the commands and the job adapters share one tracker.
pub struct PublicationHostState(pub Arc<PublicationHost>);

/// The payload of the event that asks the viewer to display one exact request.
///
/// Small on purpose: identity, revision, token and the two switches. The geometry travels
/// through the binary `splat_bytes_for_revision` response, so a 500 000 gaussian revision never
/// becomes a JSON payload.
pub fn request_payload(request: &PublicationRequest, file_name: &str, point_count: usize) -> serde_json::Value {
    json!({
        "contract_version": PUBLICATION_CONTRACT_VERSION,
        "document_id": request.document_id,
        "revision": request.revision,
        "token": request.token,
        "source": request.source.as_str(),
        "frame": request.frame,
        "file_name": file_name,
        "point_count": point_count,
    })
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
        "pending": status.pending.as_ref().map(|request| json!({
            "revision": request.revision,
            "token": request.token,
            "source": request.source.as_str(),
            "frame": request.frame,
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

/// True for the outcome that means the requested revision did *not* reach the screen.
pub fn needs_attention(outcome: &PublicationOutcome) -> bool {
    matches!(
        outcome,
        PublicationOutcome::Failed(_) | PublicationOutcome::TimedOut | PublicationOutcome::Skipped { .. }
    )
}

/// Tauri command: the status of one document's publication, or of every tracked document.
#[tauri::command]
pub fn publication_status(
    host: tauri::State<'_, PublicationHostState>,
    document_id: Option<String>,
) -> Result<serde_json::Value, String> {
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
            }))
        }
    }
}

/// Tauri command: renderer capabilities, from the viewer plus this app's own contract.
#[tauri::command]
pub fn renderer_capabilities(
    host: tauri::State<'_, PublicationHostState>,
    app: tauri::AppHandle,
) -> serde_json::Value {
    // What the viewer reports about itself, so the capabilities are the renderer's own words
    // rather than an assumption. A viewer that cannot answer is reported as not ready.
    let status = app
        .state::<crate::bridge::ViewerState>()
        .0
        .request(
            splatmcp_bridge::Method::ViewerStatus,
            serde_json::Value::Null,
            crate::viewer::VIEWER_TIMEOUT,
        )
        .ok()
        .and_then(|value| serde_json::from_value::<splatmpc_status::ViewerStatus>(value).ok());
    let (displayed_revision, point_count) = match status {
        Some(status) => (
            status
                .document
                .as_ref()
                .map(|document| document.revision),
            status.point_count,
        ),
        None => (None, 0),
    };
    capabilities_json(&host.0.capabilities(displayed_revision, point_count))
}

/// A local alias so the viewer status type is named once, in the capabilities command.
mod splatmpc_status {
    /// The viewer's own report of what it is showing.
    pub type ViewerStatus = splatmcp_bridge::ViewerStatus;
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
    use splatmcp_core::{DocumentId, PublicationTracker};

    fn handle() -> DocumentHandle {
        DocumentHandle::new(DocumentId::mint(0x4f2a, 1), 4)
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
        assert!(encoded["summary"].as_str().unwrap().contains("committing nothing") == false);
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

        let status = host
            .acknowledge("doc-4f2a-1", 4, request.token)
            .unwrap();
        assert!(status.is_current());
        assert_eq!(status.displayed_revision, Some(4));
        let encoded = status_json(&status);
        assert_eq!(encoded["last"]["outcome"], "displayed");
    }

    #[test]
    fn a_stale_acknowledgement_is_refused_and_the_newest_keeps_the_screen() {
        let host = PublicationHost::new(1_000);
        let first = host
            .begin(&handle(), PublicationSource::Committed, true)
            .unwrap();
        host.tracker().committed("doc-4f2a-1", 5).unwrap();
        let second = host
            .tracker()
            .begin("doc-4f2a-1", 5, PublicationSource::Committed, false)
            .unwrap();
        let error = host
            .acknowledge("doc-4f2a-1", 4, first.token)
            .unwrap_err();
        assert_eq!(error.code(), "stale_acknowledgement");
        assert!(needs_attention(&PublicationOutcome::TimedOut));
        let status = host
            .acknowledge("doc-4f2a-1", 5, second.token)
            .unwrap();
        assert_eq!(status.displayed_revision, Some(5));
        assert!(status.skipped.contains(&4));
    }

    #[test]
    fn a_failed_publication_keeps_the_old_frame_and_reports_the_failure() {
        let host = PublicationHost::new(1_000);
        let shown = host
            .begin(&handle(), PublicationSource::Committed, true)
            .unwrap();
        host.acknowledge("doc-4f2a-1", 4, shown.token).unwrap();
        host.tracker().committed("doc-4f2a-1", 5).unwrap();
        host.tracker()
            .begin("doc-4f2a-1", 5, PublicationSource::Committed, false)
            .unwrap();
        let status = host.fail("doc-4f2a-1", 5, "parse failed").unwrap();
        assert_eq!(status.displayed_revision, Some(4), "the old frame stays");
        assert_eq!(status.committed_revision, Some(5));
        let encoded = status_json(&status);
        assert_eq!(encoded["failures"][0]["revision"], 5);
        assert_eq!(encoded["failures"][0]["reason"], "parse failed");
        assert!(encoded["summary"].as_str().unwrap().contains("last request failed"));
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
