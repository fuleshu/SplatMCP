//! Tests of the publication state machine.
//!
//! These are the cases the task calls out, plus the ones a reviewer found: a failed publication
//! must leave the previous model displayed and report the new revision honestly; rapidly
//! published revisions A/B/C under a delayed A must end on C with no false acknowledgement for
//! B; a hidden commit must move the committed revision without claiming to be displayed; a
//! stalled publication must expire; and switching documents must move the screen.

use super::tracker::{ACK_TIMEOUT_MS, MAX_FAILURES, MAX_HISTORY};
use super::*;

/// A tracker with a fixed clock and a timeout the tests can drive.
fn tracker() -> PublicationTracker {
    PublicationTracker::new()
}

/// Starts a publication at `now`, with the contract timeout.
fn begin(
    tracker: &PublicationTracker,
    now: u64,
    document_id: &str,
    revision: u64,
    source: PublicationSource,
    frame: bool,
) -> Result<PublicationRequest, PublicationError> {
    tracker.begin_at(now, ACK_TIMEOUT_MS, document_id, revision, source, frame)
}

/// Acknowledges a publication at `now`.
fn acknowledge(
    tracker: &PublicationTracker,
    now: u64,
    document_id: &str,
    revision: u64,
    token: u64,
) -> Result<PublicationStatus, PublicationError> {
    tracker.acknowledge_at(now, ACK_TIMEOUT_MS, document_id, revision, token)
}

/// Reads a status at `now`.
fn status(
    tracker: &PublicationTracker,
    now: u64,
    document_id: &str,
) -> Result<Option<PublicationStatus>, PublicationError> {
    tracker.status_at(now, ACK_TIMEOUT_MS, document_id)
}

/// Records a failed publication at `now`.
fn fail(
    tracker: &PublicationTracker,
    now: u64,
    document_id: &str,
    revision: u64,
    reason: &str,
) -> Result<PublicationStatus, PublicationError> {
    tracker.fail_at(now, ACK_TIMEOUT_MS, document_id, revision, reason)
}

#[test]
fn committing_a_revision_never_claims_it_is_displayed() {
    let tracker = tracker();
    tracker.committed("doc-1-1", 4).unwrap();
    let read = status(&tracker, 0, "doc-1-1").unwrap().unwrap();
    assert_eq!(read.committed_revision, Some(4));
    assert_eq!(read.displayed_revision, None, "nothing has been drawn yet");
    assert!(read.display_lagging);
    assert!(!read.is_current());
    assert!(read.summary().contains("display lagging"));

    // The acknowledgement is what makes it displayed, and it must name the request.
    let request = begin(&tracker, 0, "doc-1-1", 4, PublicationSource::Committed, false).unwrap();
    assert_eq!(request.token, 1);
    let read = acknowledge(&tracker, 0, "doc-1-1", 4, request.token).unwrap();
    assert_eq!(read.displayed_revision, Some(4));
    assert!(!read.display_lagging);
    assert!(read.is_current());
    assert!(read.pending.is_none());
    assert_eq!(
        read.last.as_ref().map(|(_, outcome)| outcome.clone()),
        Some(PublicationOutcome::Displayed)
    );
}

#[test]
fn a_hidden_commit_moves_the_committed_revision_and_is_not_reported_as_current() {
    // The reported defect: a `display:false` commit left the status on the previous revision
    // and still claimed the display was current.
    let tracker = tracker();
    let shown = begin(&tracker, 0, "doc-1-1", 2, PublicationSource::Committed, true).unwrap();
    acknowledge(&tracker, 0, "doc-1-1", 2, shown.token).unwrap();
    assert!(status(&tracker, 0, "doc-1-1").unwrap().unwrap().is_current());

    // A hidden commit of revision 3: recorded, never published.
    tracker.committed("doc-1-1", 3).unwrap();
    let read = status(&tracker, 0, "doc-1-1").unwrap().unwrap();
    assert_eq!(read.committed_revision, Some(3));
    assert_eq!(read.displayed_revision, Some(2));
    assert!(!read.is_current());
    assert!(read.display_lagging);
    assert!(read.pending.is_none(), "a hidden commit publishes nothing");
}

#[test]
fn a_failed_publication_keeps_the_previous_model_and_reports_the_new_revision() {
    let tracker = tracker();
    // Revision 3 is on screen.
    tracker.committed("doc-1-1", 3).unwrap();
    let first = begin(&tracker, 0, "doc-1-1", 3, PublicationSource::Committed, true).unwrap();
    acknowledge(&tracker, 0, "doc-1-1", 3, first.token).unwrap();

    // Revision 4 is committed but the viewer cannot prepare it.
    tracker.committed("doc-1-1", 4).unwrap();
    let second = begin(&tracker, 0, "doc-1-1", 4, PublicationSource::Committed, false).unwrap();
    let read = fail(&tracker, 0, "doc-1-1", 4, "GPU upload failed").unwrap();

    // The old revision stays displayed - it is not blanked out - and the failure is reported.
    assert_eq!(read.displayed_revision, Some(3));
    assert_eq!(read.committed_revision, Some(4));
    assert!(read.display_lagging);
    assert!(read.pending.is_none(), "the failed request left flight");
    assert_eq!(read.failures.len(), 1);
    assert_eq!(read.failures[0].0, 4);
    assert!(read.failures[0].1.contains("GPU upload failed"));
    assert!(read.summary().contains("last request failed"));
    assert_eq!(second.revision, 4);
}

#[test]
fn rapidly_published_revisions_end_on_the_newest_and_never_acknowledge_a_skipped_one() {
    let tracker = tracker();
    let a = begin(&tracker, 0, "doc-1-1", 1, PublicationSource::Committed, true).unwrap();
    let b = begin(&tracker, 0, "doc-1-1", 2, PublicationSource::Committed, false).unwrap();
    let c = begin(&tracker, 0, "doc-1-1", 3, PublicationSource::Committed, false).unwrap();
    assert!(a.token < b.token && b.token < c.token);

    // A finishes last: its acknowledgement is refused, recorded, and cannot claim the screen.
    let error = acknowledge(&tracker, 0, "doc-1-1", 1, a.token).unwrap_err();
    assert_eq!(error.code(), "stale_acknowledgement");
    assert!(error.to_string().contains("token 3"), "{error}");
    let read = status(&tracker, 0, "doc-1-1").unwrap().unwrap();
    assert_eq!(read.displayed_revision, None);
    assert_eq!(read.pending.as_ref().map(|p| p.revision), Some(3));
    assert_eq!(read.skipped, vec![2, 1], "B and A were superseded, newest first");

    // C is the one that can be acknowledged, and it is what ends up displayed.
    let read = acknowledge(&tracker, 0, "doc-1-1", 3, c.token).unwrap();
    assert_eq!(read.displayed_revision, Some(3));
    assert!(read.pending.is_none());
    assert!(read.skipped.contains(&2), "B is still reported as skipped");
}

#[test]
fn a_skipped_revision_is_never_reported_as_displayed() {
    let tracker = tracker();
    let b = begin(&tracker, 0, "doc-1-1", 2, PublicationSource::Committed, false).unwrap();
    begin(&tracker, 0, "doc-1-1", 3, PublicationSource::Committed, false).unwrap();
    // B's late acknowledgement must not become the displayed revision.
    assert!(acknowledge(&tracker, 0, "doc-1-1", 2, b.token).is_err());
    let read = status(&tracker, 0, "doc-1-1").unwrap().unwrap();
    assert_ne!(read.displayed_revision, Some(2));
    assert!(read.skipped.contains(&2));
}

#[test]
fn a_stalled_publication_expires_rather_than_staying_pending() {
    let tracker = tracker();
    let request = begin(&tracker, 1_000, "doc-1-1", 4, PublicationSource::Committed, false).unwrap();
    // Inside the timeout it is honestly pending, and the status says when it started.
    let read = status(&tracker, 1_500, "doc-1-1").unwrap().unwrap();
    assert_eq!(read.pending.as_ref().map(|p| p.token), Some(request.token));
    assert_eq!(read.pending.as_ref().map(|p| p.started_at_ms), Some(1_000));

    // Past it, a read applies the timeout: a stall is reported as a stall.
    let read = status(&tracker, 1_000 + ACK_TIMEOUT_MS + 1, "doc-1-1")
        .unwrap()
        .unwrap();
    assert!(read.pending.is_none());
    assert_eq!(
        read.last.as_ref().map(|(_, outcome)| outcome.clone()),
        Some(PublicationOutcome::TimedOut)
    );
}

#[test]
fn switching_documents_moves_the_screen() {
    let tracker = tracker();
    // A is displayed at revision 7.
    let a = begin(&tracker, 0, "doc-1-1", 7, PublicationSource::Committed, true).unwrap();
    acknowledge(&tracker, 0, "doc-1-1", 7, a.token).unwrap();
    assert_eq!(tracker.active_document().unwrap().as_deref(), Some("doc-1-1"));

    // B is displayed at revision 1: it owns the screen now, and A no longer does.
    let b = begin(&tracker, 0, "doc-1-2", 1, PublicationSource::Committed, true).unwrap();
    let read_b = acknowledge(&tracker, 0, "doc-1-2", 1, b.token).unwrap();
    assert_eq!(read_b.displayed_revision, Some(1));
    assert_eq!(tracker.active_document().unwrap().as_deref(), Some("doc-1-2"));

    let read_a = status(&tracker, 0, "doc-1-1").unwrap().unwrap();
    assert_eq!(read_a.displayed_revision, None, "A is not on screen any more");
    assert_eq!(read_a.committed_revision, Some(7));
    assert!(read_a.display_lagging);

    // A is not the active document, so it can be published again without being "already
    // displayed" - and doing so takes the screen back.
    let again = begin(&tracker, 0, "doc-1-1", 7, PublicationSource::Committed, false).unwrap();
    acknowledge(&tracker, 0, "doc-1-1", 7, again.token).unwrap();
    assert_eq!(tracker.active_document().unwrap().as_deref(), Some("doc-1-1"));
    assert_eq!(
        status(&tracker, 0, "doc-1-2").unwrap().unwrap().displayed_revision,
        None
    );
}

#[test]
fn a_document_with_a_commit_but_no_publication_says_another_document_is_displayed() {
    let tracker = tracker();
    let a = begin(&tracker, 0, "doc-1-1", 1, PublicationSource::Committed, true).unwrap();
    acknowledge(&tracker, 0, "doc-1-1", 1, a.token).unwrap();
    tracker.committed("doc-1-2", 3).unwrap();

    let read = status(&tracker, 0, "doc-1-2").unwrap().unwrap();
    assert_eq!(read.displayed_revision, None);
    assert!(read.displayed_elsewhere);
    assert!(read.summary().contains("another document is displayed"));
    // The active document itself is not "displayed elsewhere".
    assert!(!status(&tracker, 0, "doc-1-1").unwrap().unwrap().displayed_elsewhere);
}

#[test]
fn a_preview_candidate_is_a_frame_not_a_revision() {
    let tracker = tracker();
    tracker.committed("doc-1-1", 5).unwrap();
    let shown = begin(&tracker, 0, "doc-1-1", 5, PublicationSource::Committed, true).unwrap();
    acknowledge(&tracker, 0, "doc-1-1", 5, shown.token).unwrap();

    // A dry-run candidate is displayed, but the document's displayed *revision* is unchanged:
    // a preview is not a revision, and the app must not pretend it is.
    let preview = begin(&tracker, 0, "doc-1-1", 5, PublicationSource::Preview, false).unwrap();
    assert_eq!(preview.source, PublicationSource::Preview);
    let read = acknowledge(&tracker, 0, "doc-1-1", 5, preview.token).unwrap();
    assert_eq!(read.displayed_revision, Some(5), "the committed revision stays");
    assert_eq!(
        read.last.as_ref().map(|(_, outcome)| outcome.clone()),
        Some(PublicationOutcome::Displayed)
    );

    // A preview with no revision can never be started.
    let error = begin(&tracker, 0, "doc-1-3", 0, PublicationSource::Preview, false).unwrap_err();
    assert_eq!(error.code(), "preview_is_not_a_revision");
}

#[test]
fn re_publishing_the_displayed_revision_is_refused() {
    let tracker = tracker();
    let request = begin(&tracker, 0, "doc-1-1", 7, PublicationSource::Committed, false).unwrap();
    acknowledge(&tracker, 0, "doc-1-1", 7, request.token).unwrap();
    let error = begin(&tracker, 0, "doc-1-1", 7, PublicationSource::Committed, false).unwrap_err();
    assert_eq!(error.code(), "already_displayed");
    // Nothing was disturbed: the request in flight is still none, and 7 is still displayed.
    let read = status(&tracker, 0, "doc-1-1").unwrap().unwrap();
    assert_eq!(read.displayed_revision, Some(7));
    assert!(read.pending.is_none());
}

#[test]
fn a_timeout_is_an_outcome_and_leaves_the_displayed_revision_alone() {
    let tracker = tracker();
    let shown = begin(&tracker, 0, "doc-1-1", 2, PublicationSource::Committed, false).unwrap();
    acknowledge(&tracker, 0, "doc-1-1", 2, shown.token).unwrap();
    let slow = begin(&tracker, 0, "doc-1-1", 3, PublicationSource::Committed, false).unwrap();
    let timed_out = tracker.time_out("doc-1-1").unwrap().unwrap();
    assert_eq!(timed_out.token, slow.token);
    let read = status(&tracker, 0, "doc-1-1").unwrap().unwrap();
    assert_eq!(read.displayed_revision, Some(2));
    assert_eq!(
        read.last.as_ref().map(|(_, outcome)| outcome.clone()),
        Some(PublicationOutcome::TimedOut)
    );
    assert!(read.pending.is_none());
    // Timing out again is a no-op: there is nothing in flight.
    assert!(tracker.time_out("doc-1-1").unwrap().is_none());
}

#[test]
fn a_failure_for_something_not_in_flight_does_not_drop_the_real_request() {
    let tracker = tracker();
    let pending = begin(&tracker, 0, "doc-1-1", 9, PublicationSource::Committed, false).unwrap();
    // A failure report for an older revision arrives late: the new request must survive it.
    let read = fail(&tracker, 0, "doc-1-1", 4, "stale report").unwrap();
    assert_eq!(read.pending.as_ref().map(|p| p.token), Some(pending.token));
    assert!(read.failures.iter().any(|(revision, _)| *revision == 4));
    // The real request can still be acknowledged.
    let read = acknowledge(&tracker, 0, "doc-1-1", 9, pending.token).unwrap();
    assert_eq!(read.displayed_revision, Some(9));
}

#[test]
fn one_document_never_disturbs_another_request_in_flight() {
    let tracker = tracker();
    let one = begin(&tracker, 0, "doc-1-1", 1, PublicationSource::Committed, true).unwrap();
    let other = begin(&tracker, 0, "doc-1-2", 1, PublicationSource::Committed, true).unwrap();
    acknowledge(&tracker, 0, "doc-1-2", 1, other.token).unwrap();
    // The first document's request is still in flight, and a foreign acknowledgement is refused.
    let error = acknowledge(&tracker, 0, "doc-1-2", 1, one.token).unwrap_err();
    assert_eq!(error.code(), "stale_acknowledgement");
    let read = status(&tracker, 0, "doc-1-1").unwrap().unwrap();
    assert_eq!(
        read.pending.as_ref().map(|p| p.document_id.as_str()),
        Some("doc-1-1")
    );
    assert_eq!(read.displayed_revision, None);

    let all = tracker.all().unwrap();
    assert_eq!(all.len(), 2);
    tracker.forget("doc-1-1").unwrap();
    assert_eq!(tracker.all().unwrap().len(), 1);
    assert!(status(&tracker, 0, "doc-1-1").unwrap().is_none());
}

#[test]
fn history_is_bounded_so_a_long_edit_sequence_stays_small() {
    let tracker = tracker();
    let mut last_token = 0;
    for revision in 1..=(MAX_HISTORY as u64 + 10) {
        let request = begin(&tracker, 0, "doc-1-1", revision, PublicationSource::Committed, false)
            .unwrap();
        last_token = request.token;
    }
    let read = status(&tracker, 0, "doc-1-1").unwrap().unwrap();
    assert_eq!(read.skipped.len(), MAX_HISTORY);
    assert_eq!(read.skipped[0], MAX_HISTORY as u64 + 9);
    acknowledge(&tracker, 0, "doc-1-1", MAX_HISTORY as u64 + 10, last_token).unwrap();
    assert_eq!(
        status(&tracker, 0, "doc-1-1").unwrap().unwrap().displayed_revision,
        Some(MAX_HISTORY as u64 + 10)
    );

    // Failures are bounded too.
    for revision in 1..=(MAX_FAILURES as u64 + 5) {
        let request = begin(
            &tracker,
            0,
            "doc-1-2",
            revision + 100,
            PublicationSource::Committed,
            false,
        )
        .unwrap();
        fail(&tracker, 0, "doc-1-2", request.revision, &format!("failure {revision}")).unwrap();
    }
    let read = status(&tracker, 0, "doc-1-2").unwrap().unwrap();
    assert_eq!(read.failures.len(), MAX_FAILURES);
}

#[test]
fn a_closed_document_forgets_its_state() {
    let tracker = tracker();
    tracker.committed("doc-1-1", 3).unwrap();
    let request = begin(&tracker, 0, "doc-1-1", 3, PublicationSource::Committed, false).unwrap();
    acknowledge(&tracker, 0, "doc-1-1", 3, request.token).unwrap();
    assert_eq!(tracker.active_document().unwrap().as_deref(), Some("doc-1-1"));
    tracker.forget("doc-1-1").unwrap();
    assert!(tracker.active_document().unwrap().is_none());

    let mut entry = DocumentPublication::new("doc-1-1");
    entry.committed(3);
    entry.clear();
    let read = entry.status();
    assert_eq!(read.committed_revision, None);
    assert_eq!(read.displayed_revision, None);
    assert!(read.skipped.is_empty() && read.failures.is_empty());
    assert!(!read.display_lagging);
}

#[test]
fn renderer_capabilities_state_what_the_seam_can_do() {
    let capabilities = RendererCapabilities::of(Some(4), 500_000, ACK_TIMEOUT_MS);
    assert!(capabilities.revision_addressed);
    assert_eq!(capabilities.transport, "tauri_binary_response");
    assert_eq!(capabilities.ack_timeout_ms, ACK_TIMEOUT_MS);
    assert!(capabilities.has_splat);
    assert!(capabilities.summary().contains("500000 gaussians"));
    // Nothing displayed: the capabilities still say what the seam supports.
    let empty = RendererCapabilities::of(None, 0, ACK_TIMEOUT_MS);
    assert!(!empty.has_splat);
    assert!(empty.revision_addressed);
}
