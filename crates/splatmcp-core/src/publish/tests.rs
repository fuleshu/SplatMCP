//! Tests of the publication state machine.
//!
//! These are the cases the task calls out: a failed publication must leave the previous model
//! displayed and report the new revision honestly; rapidly published revisions A/B/C under a
//! delayed A must end on C with no false acknowledgement for B; and the committed revision must
//! stay distinct from the displayed one throughout.

use super::tracker::{MAX_FAILURES, MAX_HISTORY};
use super::*;

fn tracker() -> PublicationTracker {
    PublicationTracker::new()
}

#[test]
fn committing_a_revision_never_claims_it_is_displayed() {
    let tracker = tracker();
    tracker.committed("doc-1-1", 4).unwrap();
    let status = tracker.status("doc-1-1").unwrap().unwrap();
    assert_eq!(status.committed_revision, Some(4));
    assert_eq!(status.displayed_revision, None, "nothing has been drawn yet");
    assert!(status.display_lagging);
    assert!(!status.is_current());
    assert!(status.summary().contains("display lagging"));

    // The acknowledgement is what makes it displayed, and it must name the request.
    let request = tracker
        .begin("doc-1-1", 4, PublicationSource::Committed, false)
        .unwrap();
    assert_eq!(request.token, 1);
    let status = tracker
        .acknowledge("doc-1-1", 4, request.token)
        .unwrap();
    assert_eq!(status.displayed_revision, Some(4));
    assert!(!status.display_lagging);
    assert!(status.is_current());
    assert!(status.pending.is_none());
    assert_eq!(
        status.last.as_ref().map(|(_, outcome)| outcome.clone()),
        Some(PublicationOutcome::Displayed)
    );
}

#[test]
fn a_failed_publication_keeps_the_previous_model_and_reports_the_new_revision() {
    let tracker = tracker();
    // Revision 3 is on screen.
    tracker.committed("doc-1-1", 3).unwrap();
    let first = tracker
        .begin("doc-1-1", 3, PublicationSource::Committed, true)
        .unwrap();
    tracker
        .acknowledge("doc-1-1", 3, first.token)
        .unwrap();

    // Revision 4 is committed but the viewer cannot prepare it.
    tracker.committed("doc-1-1", 4).unwrap();
    let second = tracker
        .begin("doc-1-1", 4, PublicationSource::Committed, false)
        .unwrap();
    let status = tracker
        .fail("doc-1-1", 4, "GPU upload failed")
        .unwrap();

    // The old revision stays displayed - it is not blanked out - and the failure is reported.
    assert_eq!(status.displayed_revision, Some(3));
    assert_eq!(status.committed_revision, Some(4));
    assert!(status.display_lagging);
    assert!(status.pending.is_none(), "the failed request left flight");
    assert_eq!(status.failures.len(), 1);
    assert_eq!(status.failures[0].0, 4);
    assert!(status.failures[0].1.contains("GPU upload failed"));
    assert!(status.summary().contains("last request failed"));
    assert_eq!(second.revision, 4);
}

#[test]
fn rapidly_published_revisions_end_on_the_newest_and_never_acknowledge_a_skipped_one() {
    let tracker = tracker();
    let a = tracker
        .begin("doc-1-1", 1, PublicationSource::Committed, true)
        .unwrap();
    let b = tracker
        .begin("doc-1-1", 2, PublicationSource::Committed, false)
        .unwrap();
    let c = tracker
        .begin("doc-1-1", 3, PublicationSource::Committed, false)
        .unwrap();
    assert!(a.token < b.token && b.token < c.token);

    // A finishes last: its acknowledgement is refused, recorded, and cannot claim the screen.
    let error = tracker.acknowledge("doc-1-1", 1, a.token).unwrap_err();
    assert_eq!(error.code(), "stale_acknowledgement");
    assert!(error.to_string().contains("token 3"), "{error}");
    let status = tracker.status("doc-1-1").unwrap().unwrap();
    assert_eq!(status.displayed_revision, None);
    assert_eq!(status.pending.as_ref().map(|p| p.revision), Some(3));
    assert_eq!(status.skipped, vec![2, 1], "B and A were superseded, newest first");

    // C is the one that can be acknowledged, and it is what ends up displayed.
    let status = tracker.acknowledge("doc-1-1", 3, c.token).unwrap();
    assert_eq!(status.displayed_revision, Some(3));
    assert!(status.pending.is_none());
    assert!(status.skipped.contains(&2), "B is still reported as skipped");
}

#[test]
fn a_skipped_revision_is_never_reported_as_displayed() {
    let tracker = tracker();
    let b = tracker
        .begin("doc-1-1", 2, PublicationSource::Committed, false)
        .unwrap();
    tracker
        .begin("doc-1-1", 3, PublicationSource::Committed, false)
        .unwrap();
    // B's late acknowledgement must not become the displayed revision.
    assert!(tracker.acknowledge("doc-1-1", 2, b.token).is_err());
    let status = tracker.status("doc-1-1").unwrap().unwrap();
    assert_ne!(status.displayed_revision, Some(2));
    assert!(status.skipped.contains(&2));
}

#[test]
fn a_preview_candidate_is_a_frame_not_a_revision() {
    let tracker = tracker();
    tracker.committed("doc-1-1", 5).unwrap();
    let shown = tracker
        .begin("doc-1-1", 5, PublicationSource::Committed, true)
        .unwrap();
    tracker
        .acknowledge("doc-1-1", 5, shown.token)
        .unwrap();

    // A dry-run candidate is displayed, but the document's displayed *revision* is unchanged:
    // a preview is not a revision, and the app must not pretend it is.
    let preview = tracker
        .begin("doc-1-1", 5, PublicationSource::Preview, false)
        .unwrap();
    assert_eq!(preview.source, PublicationSource::Preview);
    let status = tracker
        .acknowledge("doc-1-1", 5, preview.token)
        .unwrap();
    assert_eq!(status.displayed_revision, Some(5), "the committed revision stays");
    assert_eq!(
        status.last.as_ref().map(|(_, outcome)| outcome.clone()),
        Some(PublicationOutcome::Displayed)
    );

    // A preview with no revision can never be started.
    let error = tracker
        .begin("doc-1-2", 0, PublicationSource::Preview, false)
        .unwrap_err();
    assert_eq!(error.code(), "preview_is_not_a_revision");
}

#[test]
fn re_publishing_the_displayed_revision_is_refused() {
    let tracker = tracker();
    let request = tracker
        .begin("doc-1-1", 7, PublicationSource::Committed, false)
        .unwrap();
    tracker.acknowledge("doc-1-1", 7, request.token).unwrap();
    let error = tracker
        .begin("doc-1-1", 7, PublicationSource::Committed, false)
        .unwrap_err();
    assert_eq!(error.code(), "already_displayed");
    // Nothing was disturbed: the request in flight is still none, and 7 is still displayed.
    let status = tracker.status("doc-1-1").unwrap().unwrap();
    assert_eq!(status.displayed_revision, Some(7));
    assert!(status.pending.is_none());
}

#[test]
fn a_timeout_is_an_outcome_and_leaves_the_displayed_revision_alone() {
    let tracker = tracker();
    let shown = tracker
        .begin("doc-1-1", 2, PublicationSource::Committed, false)
        .unwrap();
    tracker.acknowledge("doc-1-1", 2, shown.token).unwrap();
    let slow = tracker
        .begin("doc-1-1", 3, PublicationSource::Committed, false)
        .unwrap();
    let timed_out = tracker.time_out("doc-1-1").unwrap().unwrap();
    assert_eq!(timed_out.token, slow.token);
    let status = tracker.status("doc-1-1").unwrap().unwrap();
    assert_eq!(status.displayed_revision, Some(2));
    assert_eq!(
        status.last.as_ref().map(|(_, outcome)| outcome.clone()),
        Some(PublicationOutcome::TimedOut)
    );
    assert!(status.pending.is_none());
    // Timing out again is a no-op: there is nothing in flight.
    assert!(tracker.time_out("doc-1-1").unwrap().is_none());
}

#[test]
fn a_failure_for_something_not_in_flight_does_not_drop_the_real_request() {
    let tracker = tracker();
    let pending = tracker
        .begin("doc-1-1", 9, PublicationSource::Committed, false)
        .unwrap();
    // A failure report for an older revision arrives late: the new request must survive it.
    let status = tracker.fail("doc-1-1", 4, "stale report").unwrap();
    assert_eq!(status.pending.as_ref().map(|p| p.token), Some(pending.token));
    assert!(status.failures.iter().any(|(revision, _)| *revision == 4));
    // The real request can still be acknowledged.
    let status = tracker.acknowledge("doc-1-1", 9, pending.token).unwrap();
    assert_eq!(status.displayed_revision, Some(9));
}

#[test]
fn one_document_never_disturbs_another() {
    let tracker = tracker();
    let one = tracker
        .begin("doc-1-1", 1, PublicationSource::Committed, true)
        .unwrap();
    let other = tracker
        .begin("doc-1-2", 1, PublicationSource::Committed, true)
        .unwrap();
    tracker.acknowledge("doc-1-2", 1, other.token).unwrap();
    // The first document's request is still in flight, and a foreign acknowledgement is refused.
    let error = tracker.acknowledge("doc-1-2", 1, one.token).unwrap_err();
    assert_eq!(error.code(), "stale_acknowledgement");
    let status = tracker.status("doc-1-1").unwrap().unwrap();
    assert_eq!(status.pending.as_ref().map(|p| p.document_id.as_str()), Some("doc-1-1"));
    assert_eq!(status.displayed_revision, None);

    let all = tracker.all().unwrap();
    assert_eq!(all.len(), 2);
    tracker.forget("doc-1-1").unwrap();
    assert_eq!(tracker.all().unwrap().len(), 1);
    assert!(tracker.status("doc-1-1").unwrap().is_none());
}

#[test]
fn history_is_bounded_so_a_long_edit_sequence_stays_small() {
    let tracker = tracker();
    let mut last_token = 0;
    for revision in 1..=(MAX_HISTORY as u64 + 10) {
        let request = tracker
            .begin("doc-1-1", revision, PublicationSource::Committed, false)
            .unwrap();
        last_token = request.token;
    }
    let status = tracker.status("doc-1-1").unwrap().unwrap();
    assert_eq!(status.skipped.len(), MAX_HISTORY);
    assert_eq!(status.skipped[0], MAX_HISTORY as u64 + 9);
    tracker.acknowledge("doc-1-1", MAX_HISTORY as u64 + 10, last_token).unwrap();
    assert_eq!(
        tracker.status("doc-1-1").unwrap().unwrap().displayed_revision,
        Some(MAX_HISTORY as u64 + 10)
    );

    // Failures are bounded too.
    for revision in 1..=(MAX_FAILURES as u64 + 5) {
        let request = tracker
            .begin("doc-1-2", revision + 100, PublicationSource::Committed, false)
            .unwrap();
        tracker
            .fail("doc-1-2", request.revision, format!("failure {revision}"))
            .unwrap();
    }
    let status = tracker.status("doc-1-2").unwrap().unwrap();
    assert_eq!(status.failures.len(), MAX_FAILURES);
}

#[test]
fn a_closed_document_forgets_its_state() {
    let tracker = tracker();
    tracker.committed("doc-1-1", 3).unwrap();
    let request = tracker
        .begin("doc-1-1", 3, PublicationSource::Committed, false)
        .unwrap();
    tracker.acknowledge("doc-1-1", 3, request.token).unwrap();

    let mut entry = DocumentPublication::new("doc-1-1");
    entry.committed(3);
    entry.clear();
    let status = entry.status();
    assert_eq!(status.committed_revision, None);
    assert_eq!(status.displayed_revision, None);
    assert!(status.skipped.is_empty() && status.failures.is_empty());
    assert!(!status.display_lagging);
}

#[test]
fn renderer_capabilities_state_what_the_seam_can_do() {
    let capabilities = RendererCapabilities::of(Some(4), 500_000, 5_000);
    assert!(capabilities.revision_addressed);
    assert_eq!(capabilities.transport, "tauri_binary_response");
    assert!(capabilities.has_splat);
    assert!(capabilities.summary().contains("500000 gaussians"));
    // Nothing displayed: the capabilities still say what the seam supports.
    let empty = RendererCapabilities::of(None, 0, 5_000);
    assert!(!empty.has_splat);
    assert!(empty.revision_addressed);
}
