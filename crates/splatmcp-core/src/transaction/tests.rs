//! Unit tests for [`super`], kept beside the module they exercise so the
//! implementation files stay readable.

use super::*;

use crate::components::SelectionQuery;
use crate::document::{DocumentStore, Mutation, RetentionLimits};
use crate::{SplatPoint, contract::IDENTITY_QUATERNION};

fn splat(count: usize) -> Splat {
    Splat::from_points(
        (0..count)
            .map(|index| {
                SplatPoint::new(
                    [index as f32, 0.0, 0.0],
                    [0.1; 3],
                    [0.5; 3],
                    0.8,
                    IDENTITY_QUATERNION,
                )
            })
            .collect(),
    )
}

fn harness() -> (Arc<DocumentStore>, TransactionService) {
    let store = Arc::new(DocumentStore::with_session(RetentionLimits::default(), 7));
    let service = TransactionService::new(Arc::clone(&store), TransactionLimits::default());
    (store, service)
}

fn translate(by: [f32; 3]) -> BatchStep {
    BatchStep::new(EditOp::Translate { by })
}

#[test]
fn a_batch_commits_once_and_reports_every_step() {
    let (store, service) = harness();
    store.open(splat(3), Mutation::import("scene.ply"));
    let batch = EditBatch::new(vec![
        translate([0.0, 1.0, 0.0]),
        BatchStep::new(EditOp::Duplicate {
            by: [0.0, 0.0, 1.0],
        }),
    ]);
    let receipt = service
        .commit(Expected::Any, &batch, Mutation::edit("edit_batch"))
        .unwrap();
    assert!(receipt.committed);
    assert_eq!(receipt.steps.len(), 2);
    assert_eq!(receipt.steps[0].affected, 3);
    assert_eq!(receipt.point_count, 6);
    assert_eq!(receipt.document.revision, 2);
    assert_eq!(store.active_handle().unwrap().revision, 2);
    assert!(receipt.undo_available);
    assert!(!receipt.redo_available);
    // Side effects stay separate from the commit.
    assert_eq!(receipt.export, SideEffect::NotRequested);
    assert_eq!(receipt.display, SideEffect::NotRequested);
    assert!(
        receipt
            .with_export(SideEffect::Failed("disk full".to_owned()))
            .committed
    );
}

#[test]
fn a_failing_step_leaves_the_document_untouched() {
    let (store, service) = harness();
    let opened = store.open(splat(3), Mutation::import("scene.ply"));
    let batch = EditBatch::new(vec![
        translate([0.0, 1.0, 0.0]),
        BatchStep::new(EditOp::SetRadius { factor: 0.0 }),
    ]);
    let error = service
        .commit(Expected::Any, &batch, Mutation::edit("edit_batch"))
        .unwrap_err();
    assert_eq!(error.code(), "edit_failed");
    let after = store.active_handle().unwrap();
    assert_eq!(after.revision, opened.handle.revision);
    let snapshot = store.snapshot(Expected::Any).unwrap();
    assert!(
        snapshot
            .splat()
            .points
            .iter()
            .all(|point| point.position[1] == 0.0),
        "the committed revision must not carry the first step's translation"
    );
}

#[test]
fn stable_resolution_refuses_to_redirect_a_step_and_sequential_mode_is_the_documented_opt_in() {
    // Step one removes the first gaussian; step two acts on "the first gaussian". Resolving
    // step two's predicate against the source snapshot means its target no longer exists
    // after the removal, so the batch is refused instead of quietly moving the point that
    // shifted into row 0.
    let first = Some(1);
    let batch = EditBatch::new(vec![
        BatchStep::with_targets(
            EditOp::Remove,
            BatchTargets::from_selection(Selection {
                first,
                ..Selection::default()
            }),
        ),
        BatchStep::with_targets(
            EditOp::Translate {
                by: [0.0, 5.0, 0.0],
            },
            BatchTargets::from_selection(Selection {
                first,
                ..Selection::default()
            }),
        ),
    ]);

    let (store, service) = harness();
    let opened = store.open(splat(3), Mutation::import("scene.ply"));
    let error = service
        .commit(Expected::Any, &batch, Mutation::edit("edit_batch"))
        .unwrap_err();
    assert_eq!(error.code(), "edit_failed");
    assert_eq!(store.active_handle().unwrap(), opened.handle);
    assert_eq!(store.snapshot(Expected::Any).unwrap().len(), 3);

    // The same batch in sequential mode re-evaluates the second predicate against the
    // candidate, which *is* the documented opt-in behaviour.
    let (store, service) = harness();
    store.open(splat(3), Mutation::import("scene.ply"));
    let receipt = service
        .commit(
            Expected::Any,
            &batch.clone().sequential(),
            Mutation::edit("edit_batch"),
        )
        .unwrap();
    assert_eq!(receipt.point_count, 2);
    let snapshot = store.snapshot(Expected::Any).unwrap();
    assert_eq!(snapshot.splat().points[0].position, [1.0, 5.0, 0.0]);
    assert_eq!(snapshot.splat().points[1].position, [2.0, 0.0, 0.0]);
}

#[test]
fn stable_resolution_keeps_a_later_step_on_the_id_it_resolved() {
    let (store, service) = harness();
    store.open(splat(4), Mutation::import("scene.ply"));
    // Remove the row at x = 1, then translate "the gaussian that was at x = 3": it shifted
    // from row 3 to row 2 and must still be the one that moves.
    let batch = EditBatch::new(vec![
        BatchStep::with_targets(
            EditOp::Remove,
            BatchTargets::from_selection(Selection {
                within: Some(crate::edit::Box3::from_corners([0.5; 3], [1.5, 0.0, 0.0])),
                ..Selection::default()
            }),
        ),
        BatchStep::with_targets(
            EditOp::Translate {
                by: [0.0, 5.0, 0.0],
            },
            BatchTargets::from_selection(Selection {
                within: Some(crate::edit::Box3::from_corners([2.5; 3], [3.5, 0.0, 0.0])),
                ..Selection::default()
            }),
        ),
    ]);
    let receipt = service
        .commit(Expected::Any, &batch, Mutation::edit("edit_batch"))
        .unwrap();
    assert_eq!(receipt.point_count, 3);
    let snapshot = store.snapshot(Expected::Any).unwrap();
    let positions: Vec<f32> = snapshot
        .splat()
        .points
        .iter()
        .map(|point| point.position[0])
        .collect();
    assert_eq!(positions, vec![0.0, 2.0, 3.0]);
    assert_eq!(
        snapshot.splat().points[2].position[1],
        5.0,
        "the x = 3 point moved"
    );
    assert_eq!(snapshot.splat().points[1].position[1], 0.0);
    assert_eq!(snapshot.splat().points[0].position[1], 0.0);
}

#[test]
fn an_identical_retry_replays_and_different_content_conflicts() {
    let (store, service) = harness();
    store.open(splat(2), Mutation::import("scene.ply"));
    let batch = EditBatch::new(vec![translate([0.0, 1.0, 0.0])]).with_operation_id("edit-1");
    let first = service
        .commit(Expected::Any, &batch, Mutation::edit("edit_batch"))
        .unwrap();
    assert_eq!(first.document.revision, 2);
    assert!(!first.replayed);

    let retry = service
        .commit(Expected::Any, &batch, Mutation::edit("edit_batch"))
        .unwrap();
    assert!(retry.replayed);
    assert_eq!(retry.document, first.document);
    assert_eq!(
        store.active_handle().unwrap().revision,
        2,
        "a replay must not commit again"
    );

    let different = EditBatch::new(vec![translate([0.0, 9.0, 0.0])]).with_operation_id("edit-1");
    let error = service
        .commit(Expected::Any, &different, Mutation::edit("edit_batch"))
        .unwrap_err();
    assert_eq!(error.code(), "operation_conflict");
    assert_eq!(store.active_handle().unwrap().revision, 2);
}

#[test]
fn a_stale_revision_conflicts_and_a_hold_is_taken_at_request_time() {
    let (store, service) = harness();
    let opened = store.open(splat(2), Mutation::import("scene.ply"));
    let batch = EditBatch::new(vec![translate([0.0, 1.0, 0.0])]);
    service
        .commit(Expected::Any, &batch, Mutation::edit("edit_batch"))
        .unwrap();
    let error = service
        .commit(
            Expected::Handle(opened.handle.clone()),
            &batch,
            Mutation::edit("edit_batch"),
        )
        .unwrap_err();
    assert_eq!(error.code(), "document_conflict");
    assert_eq!(error.current().unwrap().revision, 2);
}

#[test]
fn a_preview_does_not_change_the_document_and_a_stale_preview_is_refused() {
    let (store, service) = harness();
    let opened = store.open(splat(3), Mutation::import("scene.ply"));
    let batch = EditBatch::new(vec![
        translate([0.0, 2.0, 0.0]),
        BatchStep::with_targets(
            EditOp::Remove,
            BatchTargets::from_selection(Selection {
                first: Some(1),
                ..Selection::default()
            }),
        ),
    ]);
    let outcome = service.preview(Expected::Any, &batch).unwrap();
    assert_eq!(outcome.report.points_before, 3);
    assert_eq!(outcome.report.points_after, 2);
    assert_eq!(outcome.report.steps.len(), 2);
    assert_eq!(outcome.report.steps[1].affected, 1);
    assert!(outcome.report.bounds_after.is_some());
    assert_eq!(
        store.active_handle().unwrap().revision,
        opened.handle.revision,
        "previewing must not commit"
    );
    // The candidate is readable without being displayed.
    let candidate = service.preview_snapshot(outcome.preview_id).unwrap();
    assert_eq!(candidate.splat.len(), 2);

    // A newer revision makes the preview stale.
    service
        .commit(
            Expected::Any,
            &EditBatch::new(vec![translate([1.0, 0.0, 0.0])]),
            Mutation::edit("edit_batch"),
        )
        .unwrap();
    let error = service
        .commit_preview(outcome.preview_id, Expected::Any, None)
        .unwrap_err();
    assert_eq!(error.code(), "preview_conflict");

    // Committing from the required revision works and reports the preview it came from.
    let store2 = Arc::new(DocumentStore::with_session(RetentionLimits::default(), 9));
    let service2 = TransactionService::new(Arc::clone(&store2), TransactionLimits::default());
    store2.open(splat(3), Mutation::import("scene.ply"));
    let outcome = service2.preview(Expected::Any, &batch).unwrap();
    let receipt = service2
        .commit_preview(outcome.preview_id, Expected::Any, None)
        .unwrap();
    assert_eq!(receipt.document.revision, 2);
    assert_eq!(receipt.preview.unwrap().preview_id, outcome.preview_id);
    assert_eq!(receipt.point_count, 2);
    assert_eq!(store2.snapshot(Expected::Any).unwrap().len(), 2);
    // The preview handle is consumed.
    assert_eq!(
        service2
            .preview_snapshot(outcome.preview_id)
            .unwrap_err()
            .code(),
        "preview_expired"
    );
}

#[test]
fn undo_and_redo_commit_new_revisions_and_a_new_edit_drops_redo() {
    let (store, service) = harness();
    let opened = store.open(splat(2), Mutation::import("scene.ply"));
    let batch = EditBatch::new(vec![translate([0.0, 1.0, 0.0])]);
    service
        .commit(Expected::Any, &batch, Mutation::edit("edit_batch"))
        .unwrap();
    assert_eq!(
        store.snapshot(Expected::Any).unwrap().splat().points[0].position[1],
        1.0
    );

    let undone = service.undo(Expected::Any).unwrap();
    assert_eq!(undone.document.revision, 3, "undo is a new revision");
    assert_eq!(
        store.snapshot(Expected::Any).unwrap().splat().points[0].position[1],
        0.0
    );
    assert!(undone.redo_available);

    let redone = service.redo(Expected::Any).unwrap();
    assert_eq!(redone.document.revision, 4);
    assert_eq!(
        store.snapshot(Expected::Any).unwrap().splat().points[0].position[1],
        1.0
    );

    // Undo again, then a new edit: redo is invalidated.
    service.undo(Expected::Any).unwrap();
    let report = service.history(Expected::Any).unwrap();
    assert!(report.redo.is_some());
    service
        .commit(
            Expected::Any,
            &EditBatch::new(vec![translate([0.0, 0.0, 3.0])]),
            Mutation::edit("edit_batch"),
        )
        .unwrap();
    let report = service.history(Expected::Any).unwrap();
    assert!(report.redo.is_none(), "a new edit invalidates redo");
    assert_eq!(
        service.redo(Expected::Any).unwrap_err().code(),
        "no_history"
    );
    assert_eq!(opened.handle.revision, 1);
}

#[test]
fn history_is_bounded_and_evicts_the_oldest_step() {
    let store = Arc::new(DocumentStore::with_session(RetentionLimits::default(), 3));
    let limits = TransactionLimits {
        max_history_entries: 2,
        max_history_bytes: usize::MAX,
        ..TransactionLimits::default()
    };
    let service = TransactionService::new(Arc::clone(&store), limits);
    store.open(splat(2), Mutation::import("scene.ply"));
    for step in 1..=3 {
        service
            .commit(
                Expected::Any,
                &EditBatch::new(vec![translate([0.0, step as f32, 0.0])]),
                Mutation::edit("edit_batch"),
            )
            .unwrap();
    }
    let report = service.history(Expected::Any).unwrap();
    assert_eq!(report.entries.len(), 2, "the oldest step was evicted");
    assert_eq!(report.entries[0].revision, 4);
    assert_eq!(report.entries[1].revision, 3);
    assert!(report.undo.is_some());
    assert!(report.retained_bytes <= report.max_bytes);
}

#[test]
fn component_metadata_changes_advance_the_revision_and_keep_membership() {
    let (store, service) = harness();
    store.open(splat(4), Mutation::import("scene.ply"));
    let created = service.create_component(Expected::Any, "hair").unwrap();
    assert_eq!(created.document.revision, 2);
    let list = service.components(Expected::Any).unwrap();
    assert_eq!(list.components.len(), 1);
    assert_eq!(list.components[0].name, "hair");
    assert!(!list.rebuilt);

    service
        .set_component_members(
            Expected::Any,
            &created.component_id,
            &SelectionQuery {
                first: Some(2),
                ..SelectionQuery::all()
            },
        )
        .unwrap();
    let list = service.components(Expected::Any).unwrap();
    assert_eq!(list.components[0].len(), 2);
    // Renaming keeps the identity.
    let renamed = service
        .rename_component(Expected::Any, &created.component_id, "hair-back")
        .unwrap();
    assert_eq!(renamed.component_id, created.component_id);
    assert_eq!(
        service.components(Expected::Any).unwrap().components[0].name,
        "hair-back"
    );
    // Removing leaves the gaussians alone.
    service
        .remove_component(Expected::Any, &created.component_id)
        .unwrap();
    let list = service.components(Expected::Any).unwrap();
    assert!(list.components.is_empty());
    assert_eq!(store.snapshot(Expected::Any).unwrap().len(), 4);
}

#[test]
fn a_selection_handle_can_target_a_batch_and_a_stale_one_is_refused() {
    let (store, service) = harness();
    store.open(splat(4), Mutation::import("scene.ply"));
    let handle = service
        .select(
            Expected::Any,
            &SelectionQuery {
                first: Some(2),
                ..SelectionQuery::all()
            },
        )
        .unwrap();
    assert_eq!(handle.count, 2);
    assert_eq!(handle.revision, 1);

    let batch = EditBatch::new(vec![BatchStep::with_targets(
        EditOp::Translate {
            by: [0.0, 7.0, 0.0],
        },
        BatchTargets {
            selection_handle: Some(handle.id),
            ..BatchTargets::all()
        },
    )]);
    service
        .commit(Expected::Any, &batch, Mutation::edit("edit_batch"))
        .unwrap();
    let snapshot = store.snapshot(Expected::Any).unwrap();
    assert_eq!(snapshot.splat().points[0].position[1], 7.0);
    assert_eq!(snapshot.splat().points[1].position[1], 7.0);
    assert_eq!(snapshot.splat().points[2].position[1], 0.0);

    // The handle was bound to revision 1, so it no longer addresses the document.
    let error = service
        .commit(Expected::Any, &batch, Mutation::edit("edit_batch"))
        .unwrap_err();
    assert_eq!(error.code(), "invalid_selection");
}

#[test]
fn a_late_retry_replays_the_recorded_outcome_even_after_the_document_moved_on() {
    let (store, service) = harness();
    store.open(splat(3), Mutation::import("scene.ply"));
    let batch = EditBatch::new(vec![BatchStep::new(EditOp::Duplicate {
        by: [0.0, 0.0, 1.0],
    })])
    .with_operation_id("recipe-late");
    let first = service
        .commit(Expected::Any, &batch, Mutation::edit("edit_batch"))
        .unwrap();
    assert_eq!(first.point_count, 6);
    assert_eq!(first.recorded.point_count, 6);
    assert_eq!(first.recorded.file_name, "scene.ply");
    let first_revision = first.document.revision;

    // Undo, redo and another edit: the document is now several revisions on, and the source
    // revision the original request started from is long gone from retention.
    service.undo(Expected::Any).unwrap();
    service.redo(Expected::Any).unwrap();
    service
        .commit(
            Expected::Any,
            &EditBatch::new(vec![BatchStep::with_targets(
                EditOp::Remove,
                BatchTargets::from_selection(Selection {
                    first: Some(3),
                    ..Selection::default()
                }),
            )]),
            Mutation::edit("edit_batch"),
        )
        .unwrap();

    // The retry still reports *what the original request did*, not what is displayed now.
    let retry = service
        .commit(Expected::Any, &batch, Mutation::edit("edit_batch"))
        .unwrap();
    assert!(retry.replayed);
    assert_eq!(retry.document, first.document);
    assert_eq!(retry.recorded, first.recorded);
    assert_eq!(retry.point_count, first.point_count);
    assert_eq!(retry.steps, first.steps);
    assert_eq!(retry.document.revision, first_revision);
    assert_eq!(store.active_handle().unwrap().revision, first_revision + 3);
}

#[test]
fn a_retry_with_an_evicted_source_snapshot_still_answers_from_the_receipt() {
    // One retained revision: the retry arrives after the source revision has been evicted and
    // after the document was replaced, which used to fail with `snapshot_expired`.
    let store = Arc::new(DocumentStore::with_session(
        RetentionLimits::new(1, usize::MAX),
        5,
    ));
    let service = TransactionService::new(Arc::clone(&store), TransactionLimits::default());
    store.open(splat(2), Mutation::import("first.ply"));
    let batch = EditBatch::new(vec![BatchStep::new(EditOp::Translate {
        by: [0.0, 1.0, 0.0],
    })])
    .with_operation_id("recipe-evicted");
    let first = service
        .commit(Expected::Any, &batch, Mutation::edit("edit_batch"))
        .unwrap();
    assert_eq!(first.document.revision, 2);

    // A different document replaces it, so neither the source nor the produced revision of the
    // original request is resolvable any more.
    store.open(splat(7), Mutation::import("second.ply"));
    let retry = service
        .commit(Expected::Any, &batch, Mutation::edit("edit_batch"))
        .unwrap();
    assert!(retry.replayed);
    assert_eq!(retry.recorded.document_id, first.recorded.document_id);
    assert_eq!(retry.recorded.revision, first.recorded.revision);
    assert_eq!(retry.point_count, first.point_count);
}

#[test]
fn an_acknowledged_side_effect_is_replayed_and_unacknowledged_display_is_not_done() {
    let (store, service) = harness();
    store.open(splat(2), Mutation::import("scene.ply"));
    let batch = EditBatch::new(vec![BatchStep::new(EditOp::Translate {
        by: [1.0, 0.0, 0.0],
    })])
    .with_operation_id("recipe-ack");
    let first = service
        .commit(Expected::Any, &batch, Mutation::edit("edit_batch"))
        .unwrap();
    // Nothing has confirmed a picture yet: `published` is not `done`.
    assert_eq!(first.display, SideEffect::NotRequested);
    assert!(service.note_side_effect(&first.document, ReceiptSlot::Display, SideEffect::Published));
    assert!(service.note_side_effect(&first.document, ReceiptSlot::Export, SideEffect::Done));

    let retry = service
        .commit(Expected::Any, &batch, Mutation::edit("edit_batch"))
        .unwrap();
    assert!(retry.replayed);
    assert_eq!(retry.display, SideEffect::Published);
    assert!(retry.display.is_published() && !retry.display.is_done());
    assert_eq!(retry.export, SideEffect::Done);

    // A later acknowledgement is what turns it into `done`.
    assert!(service.note_side_effect(&first.document, ReceiptSlot::Display, SideEffect::Done));
    assert_eq!(
        service.receipt(&first.document).unwrap().display,
        SideEffect::Done
    );
    // An unknown revision is reported as unknown rather than silently stored.
    let foreign = DocumentHandle::new(DocumentId::mint(9, 9), 1);
    assert!(!service.note_side_effect(&foreign, ReceiptSlot::Display, SideEffect::Done));
}

#[test]
fn a_preview_commit_with_an_operation_id_is_safe_to_retry() {
    let (store, service) = harness();
    store.open(splat(3), Mutation::import("scene.ply"));
    let batch = EditBatch::new(vec![BatchStep::with_targets(
        EditOp::Remove,
        BatchTargets::from_selection(Selection {
            first: Some(1),
            ..Selection::default()
        }),
    )]);
    let outcome = service.preview(Expected::Any, &batch).unwrap();
    let first = service
        .commit_preview(
            outcome.preview_id,
            Expected::Any,
            Some("preview-commit-1".to_owned()),
        )
        .unwrap();
    assert_eq!(first.point_count, 2);
    assert_eq!(first.preview.unwrap().preview_id, outcome.preview_id);
    let revision_after_commit = store.active_handle().unwrap().revision;
    assert_eq!(first.document.revision, revision_after_commit);

    // The candidate is consumed, but the *request* still has a recorded outcome.
    let retry = service
        .commit_preview(
            outcome.preview_id,
            Expected::Any,
            Some("preview-commit-1".to_owned()),
        )
        .unwrap();
    assert!(retry.replayed);
    assert_eq!(retry.document, first.document);
    assert_eq!(retry.point_count, first.point_count);
    assert_eq!(
        store.active_handle().unwrap().revision,
        revision_after_commit
    );

    // A different operation id, or none at all, is still refused: there is no candidate left.
    assert_eq!(
        service
            .commit_preview(outcome.preview_id, Expected::Any, None)
            .unwrap_err()
            .code(),
        "preview_expired"
    );
}
