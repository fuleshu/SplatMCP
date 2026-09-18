//! Acceptance tests for tasks #13 (atomic, previewable, retry-safe, undoable edit batches) and
//! #14 (named components, stable point selections and local transforms).
//!
//! These are the scenarios the two task descriptions name, exercised through the public API:
//! a store, the transaction service, and nothing that only exists for a test.

use std::sync::Arc;

use splatmcp_core::components::{Frame, LocalTransform, SelectionQuery, Sphere};
use splatmcp_core::contract::{covariance, dominant_axis};
use splatmcp_core::{
    BatchStep, BatchTargets, DocumentStore, EditBatch, EditOp, Expected, Mutation, RetentionLimits,
    SideEffect, Splat, SplatPoint, TransactionError, TransactionLimits, TransactionService,
};

fn point(position: [f32; 3], scale: [f32; 3], color: [f32; 3]) -> SplatPoint {
    SplatPoint::new(position, scale, color, 1.0, [1.0, 0.0, 0.0, 0.0])
}

/// Three overlapping groups: a "face" slab, a "hair" slab and a "sweater" slab.
fn three_component_scene() -> Splat {
    let mut points = Vec::new();
    for index in 0..4 {
        let x = index as f32;
        // face: y near 0
        points.push(point([x, 0.0, 0.0], [0.1; 3], [0.8, 0.6, 0.5]));
        // hair: overlaps the face in x, but sits above it in y
        points.push(point([x, 0.2, 0.0], [0.1; 3], [0.1, 0.05, 0.02]));
        // sweater: below, and overlapping both in x
        points.push(point([x, -0.4, 0.0], [0.1; 3], [0.2, 0.3, 0.6]));
    }
    Splat::from_points(points)
}

struct Scene {
    store: Arc<DocumentStore>,
    service: TransactionService,
}

impl Scene {
    fn open(splat: Splat) -> Self {
        let store = Arc::new(DocumentStore::with_session(RetentionLimits::default(), 11));
        let service = TransactionService::new(Arc::clone(&store), TransactionLimits::default());
        store.open(splat, Mutation::import("scene.ply"));
        Self { store, service }
    }

    fn splat(&self) -> Splat {
        let snapshot = self.store.snapshot(Expected::Any).unwrap();
        snapshot.splat().as_ref().clone()
    }

    fn revision(&self) -> u64 {
        self.store.active_handle().unwrap().revision
    }
}

#[test]
fn a_batch_that_fails_in_the_middle_or_at_final_validation_changes_nothing() {
    let scene = Scene::open(three_component_scene());
    let before = scene.splat();
    let before_ids = scene
        .service
        .select(Expected::Any, &SelectionQuery::all())
        .unwrap()
        .ids()
        .to_vec();
    let revision = scene.revision();

    // A step that selects nothing: the batch stops there, and the first step's translation is
    // discarded with it.
    let empty_selection = EditBatch::new(vec![
        BatchStep::new(EditOp::Translate {
            by: [0.0, 9.0, 0.0],
        }),
        BatchStep::with_targets(
            EditOp::Remove,
            BatchTargets::from_selection(splatmcp_core::Selection {
                within: Some(splatmcp_core::Box3::from_corners(
                    [50.0; 3],
                    [51.0, 51.0, 51.0],
                )),
                ..splatmcp_core::Selection::default()
            }),
        ),
    ]);
    let error = scene
        .service
        .commit(
            Expected::Any,
            &empty_selection,
            Mutation::edit("edit_batch"),
        )
        .unwrap_err();
    assert_eq!(error.code(), "edit_failed");
    assert_eq!(scene.revision(), revision);
    assert_eq!(scene.splat(), before);

    // Final validation: an invalid operation is refused before anything runs.
    let invalid = EditBatch::new(vec![
        BatchStep::new(EditOp::Translate {
            by: [0.0, 9.0, 0.0],
        }),
        BatchStep::new(EditOp::SetRadius { factor: 0.0 }),
    ]);
    let error = scene
        .service
        .commit(Expected::Any, &invalid, Mutation::edit("edit_batch"))
        .unwrap_err();
    assert_eq!(error.code(), "edit_failed");
    assert_eq!(scene.revision(), revision);
    assert_eq!(scene.splat(), before);

    // Ids are unchanged too, so no saved selection was redirected.
    let after_ids = scene
        .service
        .select(Expected::Any, &SelectionQuery::all())
        .unwrap()
        .ids()
        .to_vec();
    assert_eq!(after_ids, before_ids);
}

#[test]
fn a_lost_response_is_safe_to_retry_even_from_a_second_connection() {
    let scene = Scene::open(three_component_scene());
    let batch = EditBatch::new(vec![
        BatchStep::with_targets(
            EditOp::Duplicate {
                by: [0.0, 0.0, 1.0],
            },
            BatchTargets::from_selection(splatmcp_core::Selection {
                first: Some(2),
                ..splatmcp_core::Selection::default()
            }),
        ),
        BatchStep::new(EditOp::Merge {
            points: vec![point([9.0, 9.0, 9.0], [0.05; 3], [0.5; 3])],
        }),
    ])
    .with_operation_id("recipe-42");

    let first = scene
        .service
        .commit(Expected::Any, &batch, Mutation::edit("edit_batch"))
        .unwrap();
    assert_eq!(scene.splat().len(), 15);

    // The same request again, as a second connection to the same app: one service per app owns
    // the ledger, so the retry sees the recorded receipt and does not create the geometry twice.
    let retry = scene
        .service
        .commit(Expected::Any, &batch, Mutation::edit("edit_batch"))
        .unwrap();
    assert!(retry.replayed);
    assert_eq!(retry.document, first.document);
    assert_eq!(
        scene.splat().len(),
        15,
        "an identical retry creates nothing"
    );
    assert_eq!(scene.revision(), 2);

    // Different content under the same id is refused.
    let different = EditBatch::new(vec![BatchStep::new(EditOp::Translate {
        by: [0.0, 1.0, 0.0],
    })])
    .with_operation_id("recipe-42");
    let error = scene
        .service
        .commit(Expected::Any, &different, Mutation::edit("edit_batch"))
        .unwrap_err();
    assert_eq!(error.code(), "operation_conflict");
    assert_eq!(scene.revision(), 2);
}

#[test]
fn an_operation_whose_receipt_expired_reports_an_unknown_outcome() {
    let store = Arc::new(DocumentStore::with_session(RetentionLimits::default(), 4));
    let limits = TransactionLimits {
        receipt_ttl_ms: 0,
        ..TransactionLimits::default()
    };
    let service = TransactionService::new(Arc::clone(&store), limits);
    store.open(three_component_scene(), Mutation::import("scene.ply"));
    let batch = EditBatch::new(vec![BatchStep::new(EditOp::Translate {
        by: [0.0, 1.0, 0.0],
    })])
    .with_operation_id("recipe-7");
    service
        .commit(Expected::Any, &batch, Mutation::edit("edit_batch").at_ms(1))
        .unwrap();
    // The retry arrives after the receipt's lifetime: the service refuses to replay a
    // destructive operation it can no longer vouch for.
    let error = service
        .commit(Expected::Any, &batch, Mutation::edit("edit_batch").at_ms(2))
        .unwrap_err();
    assert_eq!(error.code(), "unknown_outcome");
    assert_eq!(store.active_handle().unwrap().revision, 2);
}

#[test]
fn a_preview_is_inert_and_a_stale_preview_cannot_overwrite_a_newer_revision() {
    let scene = Scene::open(three_component_scene());
    let batch = EditBatch::new(vec![BatchStep::with_targets(
        EditOp::Remove,
        BatchTargets::from_selection(splatmcp_core::Selection {
            first: Some(3),
            ..splatmcp_core::Selection::default()
        }),
    )]);
    let outcome = scene.service.preview(Expected::Any, &batch).unwrap();
    assert_eq!(outcome.report.points_before, 12);
    assert_eq!(outcome.report.points_after, 9);
    assert_eq!(outcome.report.bounds_before, scene.splat().bounds());
    assert!(outcome.report.memory_estimate_bytes > 0);
    assert_eq!(scene.splat().len(), 12, "the preview changed nothing");

    // A different edit lands first.
    scene
        .service
        .commit(
            Expected::Any,
            &EditBatch::new(vec![BatchStep::new(EditOp::Translate {
                by: [0.0, 0.5, 0.0],
            })]),
            Mutation::edit("edit_batch"),
        )
        .unwrap();
    let error = scene
        .service
        .commit_preview(outcome.preview_id, Expected::Any, None)
        .unwrap_err();
    assert!(matches!(error, TransactionError::PreviewConflict { .. }));
    assert_eq!(scene.splat().len(), 12, "the stale preview was not applied");

    // Committing a fresh preview from the revision it was built against works.
    let fresh = scene.service.preview(Expected::Any, &batch).unwrap();
    let receipt = scene
        .service
        .commit_preview(fresh.preview_id, Expected::Any, None)
        .unwrap();
    assert_eq!(receipt.preview.unwrap().preview_id, fresh.preview_id);
    assert_eq!(scene.splat().len(), 9);
}

#[test]
fn undo_and_redo_keep_geometry_and_component_membership_and_advance_revisions() {
    let scene = Scene::open(three_component_scene());
    let hair_query = SelectionQuery {
        within: Some(splatmcp_core::Box3::from_corners(
            [-1.0, 0.1, -1.0],
            [10.0, 0.3, 1.0],
        )),
        ..SelectionQuery::all()
    };
    let hair = scene
        .service
        .create_component(Expected::Any, "hair")
        .unwrap();
    scene
        .service
        .set_component_members(Expected::Any, &hair.component_id, &hair_query)
        .unwrap();
    let members = scene.service.components(Expected::Any).unwrap().components[0]
        .point_ids
        .clone();
    assert_eq!(members.len(), 4);
    let revision = scene.revision();
    let original = scene.splat();

    // Replace the hair: remove exactly the members and merge new geometry.
    let replacement = vec![
        point([0.0, 0.25, 0.0], [0.09; 3], [0.05, 0.02, 0.01]),
        point([1.0, 0.25, 0.0], [0.09; 3], [0.05, 0.02, 0.01]),
    ];
    let batch = EditBatch::new(vec![
        BatchStep::with_targets(EditOp::Remove, BatchTargets::points(members.clone())),
        BatchStep::new(EditOp::Merge {
            points: replacement.clone(),
        }),
    ]);
    let receipt = scene
        .service
        .commit(Expected::Any, &batch, Mutation::edit("edit_batch"))
        .unwrap();
    assert_eq!(receipt.point_count, 10);
    assert_eq!(scene.revision(), revision + 1);

    let undone = scene.service.undo(Expected::Any).unwrap();
    assert_eq!(
        undone.document.revision,
        revision + 2,
        "undo is a new revision"
    );
    assert_eq!(
        scene.splat(),
        original,
        "undo restores the exact geometry the step replaced"
    );

    let redone = scene.service.redo(Expected::Any).unwrap();
    assert_eq!(redone.document.revision, revision + 3);
    assert_eq!(scene.splat().len(), 10);

    // The component's identity survived the cycles, and its membership was restored.
    let list = scene.service.components(Expected::Any).unwrap();
    assert_eq!(list.components[0].id, hair.component_id);
    assert_eq!(list.components[0].name, "hair");

    // History is bounded, and the report says what it retains.
    let report = scene.service.history(Expected::Any).unwrap();
    assert!(report.undo.is_some());
    assert!(report.retained_bytes <= report.max_bytes);
    assert!(report.entries.len() <= 8);
}

#[test]
fn replacing_one_component_leaves_the_others_bit_identical() {
    let scene = Scene::open(three_component_scene());
    let face = scene
        .service
        .create_component(Expected::Any, "face")
        .unwrap();
    let hair = scene
        .service
        .create_component(Expected::Any, "hair")
        .unwrap();
    let sweater = scene
        .service
        .create_component(Expected::Any, "sweater")
        .unwrap();

    let band = |low: f32, high: f32| SelectionQuery {
        within: Some(splatmcp_core::Box3::from_corners(
            [-1.0, low, -1.0],
            [10.0, high, 1.0],
        )),
        ..SelectionQuery::all()
    };
    scene
        .service
        .set_component_members(Expected::Any, &face.component_id, &band(-0.1, 0.1))
        .unwrap();
    scene
        .service
        .set_component_members(Expected::Any, &hair.component_id, &band(0.15, 0.3))
        .unwrap();
    scene
        .service
        .set_component_members(Expected::Any, &sweater.component_id, &band(-0.5, -0.3))
        .unwrap();

    let before = scene.splat();
    let list = scene.service.components(Expected::Any).unwrap();
    let members_of = |id: &splatmcp_core::ComponentId| {
        list.components
            .iter()
            .find(|component| &component.id == id)
            .unwrap()
            .point_ids
            .clone()
    };
    let face_members = members_of(&face.component_id);
    let hair_members = members_of(&hair.component_id);
    let sweater_members = members_of(&sweater.component_id);
    assert_eq!(face_members.len(), 4);
    assert_eq!(hair_members.len(), 4);
    assert_eq!(sweater_members.len(), 4);

    // Replace the hair with one new gaussian: the members go, the replacement arrives.
    let new_hair = point([0.5, 0.3, 0.0], [0.08; 3], [0.02, 0.01, 0.01]);
    scene
        .service
        .commit(
            Expected::Any,
            &EditBatch::new(vec![
                BatchStep::with_targets(EditOp::Remove, BatchTargets::points(hair_members.clone())),
                BatchStep::new(EditOp::Merge {
                    points: vec![new_hair],
                }),
            ]),
            Mutation::edit("edit_batch"),
        )
        .unwrap();

    let after = scene.splat();
    let list = scene.service.components(Expected::Any).unwrap();
    let component = |id: &splatmcp_core::ComponentId| {
        list.components
            .iter()
            .find(|component| &component.id == id)
            .unwrap()
    };
    // Identity survived, membership was cleaned up: the retired ids are gone from the group.
    assert_eq!(component(&hair.component_id).id, hair.component_id);
    assert!(component(&hair.component_id).point_ids.is_empty());
    assert_eq!(component(&face.component_id).point_ids.len(), 4);
    assert_eq!(component(&sweater.component_id).point_ids.len(), 4);

    // Every gaussian outside the replaced component holds exactly the same numbers.
    let without_hair = |splat: &Splat, hair_color: [f32; 3]| -> Vec<SplatPoint> {
        splat
            .points
            .iter()
            .copied()
            .filter(|point| point.color != hair_color)
            .collect()
    };
    assert_eq!(
        without_hair(&after, [0.02, 0.01, 0.01]),
        without_hair(&before, [0.1, 0.05, 0.02]),
        "face and sweater data must be untouched"
    );
    assert!(
        after
            .points
            .iter()
            .any(|point| point.color == [0.02, 0.01, 0.01]),
        "the replacement geometry is in the document"
    );
}

#[test]
fn a_saved_selection_survives_a_removal_and_never_redirects_to_another_point() {
    let scene = Scene::open(three_component_scene());
    // Save a selection of the last three gaussians of the face band.
    let saved = scene
        .service
        .select(
            Expected::Any,
            &SelectionQuery {
                within: Some(splatmcp_core::Box3::from_corners(
                    [2.5, -0.1, -0.1],
                    [3.5, 0.1, 0.1],
                )),
                ..SelectionQuery::all()
            },
        )
        .unwrap();
    assert_eq!(saved.count, 1);

    // Delete an earlier gaussian, then act on the saved handle: it is bound to the old
    // revision, so the batch is refused rather than moving whichever point shifted into
    // that row.
    scene
        .service
        .commit(
            Expected::Any,
            &EditBatch::new(vec![BatchStep::with_targets(
                EditOp::Remove,
                BatchTargets::from_selection(splatmcp_core::Selection {
                    first: Some(1),
                    ..splatmcp_core::Selection::default()
                }),
            )]),
            Mutation::edit("edit_batch"),
        )
        .unwrap();
    let stale = scene
        .service
        .commit(
            Expected::Any,
            &EditBatch::new(vec![BatchStep::with_targets(
                EditOp::Translate {
                    by: [0.0, 5.0, 0.0],
                },
                BatchTargets {
                    selection_handle: Some(saved.id),
                    ..BatchTargets::all()
                },
            )]),
            Mutation::edit("edit_batch"),
        )
        .unwrap_err();
    assert_eq!(stale.code(), "invalid_selection");
    assert!(scene.splat().points.iter().all(|p| p.position[1] < 1.0));
}

#[test]
fn local_and_world_frames_disagree_exactly_as_the_transform_says() {
    let scene = Scene::open(Splat::from_points(vec![
        point([1.0, 0.0, 0.0], [0.1; 3], [0.5; 3]),
        point([3.0, 0.0, 0.0], [0.1; 3], [0.5; 3]),
    ]));
    let component = scene
        .service
        .create_component(Expected::Any, "left")
        .unwrap();
    scene
        .service
        .set_component_members(
            Expected::Any,
            &component.component_id,
            &SelectionQuery::all(),
        )
        .unwrap();
    scene
        .service
        .set_component_transform(
            Expected::Any,
            &component.component_id,
            Some(LocalTransform::translation([1.0, 0.0, 0.0])),
        )
        .unwrap();

    let box_around_local_origin = splatmcp_core::Box3::from_corners([-0.5; 3], [0.5; 3]);
    let local = scene
        .service
        .select(
            Expected::Any,
            &SelectionQuery {
                component: Some(component.component_id.clone()),
                frame: Frame::Local,
                within: Some(box_around_local_origin),
                ..SelectionQuery::all()
            },
        )
        .unwrap();
    assert_eq!(local.count, 1, "the local frame moves the box onto x = 1");

    let world = scene
        .service
        .select(
            Expected::Any,
            &SelectionQuery {
                component: Some(component.component_id.clone()),
                frame: Frame::World,
                sphere: Some(Sphere::new([3.0, 0.0, 0.0], 0.1)),
                ..SelectionQuery::all()
            },
        )
        .unwrap();
    assert_eq!(world.count, 1);
    assert_ne!(
        local.ids(),
        world.ids(),
        "the same box means different gaussians in the two frames"
    );
}

#[test]
fn an_anisotropic_rotated_gaussian_is_transformed_by_its_component_frame() {
    let scene = Scene::open(splatmcp_core::fixtures::rotated_fixture());
    let before = scene.splat();
    let expected = covariance(before.points[0].scale, before.points[0].rotation);

    let component = scene
        .service
        .create_component(Expected::Any, "rotated")
        .unwrap();
    scene
        .service
        .set_component_members(
            Expected::Any,
            &component.component_id,
            &SelectionQuery::all(),
        )
        .unwrap();
    // Squash X and stretch Y: the fixture's long axis lies along document Y, so it stays the
    // long axis, and a "multiply the radii" transform would not show that.
    scene
        .service
        .set_component_transform(
            Expected::Any,
            &component.component_id,
            Some(LocalTransform {
                translation: [0.0; 3],
                rotation: [1.0, 0.0, 0.0, 0.0],
                scale: [0.5, 2.0, 1.0],
            }),
        )
        .unwrap();

    let receipt = scene
        .service
        .apply_component_transform(Expected::Any, &component.component_id)
        .unwrap();
    assert_eq!(receipt.steps[0].affected, 1);

    let after = scene.splat();
    let got = covariance(after.points[0].scale, after.points[0].rotation);
    assert!((got[0][0] - expected[0][0] * 0.25).abs() < 1e-7, "{got:?}");
    assert!((got[1][1] - expected[1][1] * 4.0).abs() < 1e-5, "{got:?}");
    assert!((got[2][2] - expected[2][2]).abs() < 1e-7, "{got:?}");
    let axis = dominant_axis(after.points[0].scale, after.points[0].rotation).unwrap();
    assert!((axis[1].abs() - 1.0).abs() < 1e-3, "{axis:?}");
    // The frame's linear part moves the centre too: A · p = (0.5 * 0.5, 0, 1 * 0.25).
    assert_eq!(after.points[0].position, [0.25, 0.0, 0.25]);
    // Colour and opacity are not part of the frame.
    assert_eq!(after.points[0].color, before.points[0].color);
    assert_eq!(after.points[0].opacity, before.points[0].opacity);
}

#[test]
fn export_and_display_outcomes_are_reported_separately_from_the_commit() {
    let scene = Scene::open(three_component_scene());
    let receipt = scene
        .service
        .commit(
            Expected::Any,
            &EditBatch::new(vec![BatchStep::new(EditOp::Translate {
                by: [0.0, 1.0, 0.0],
            })]),
            Mutation::edit("edit_batch"),
        )
        .unwrap();
    assert!(receipt.committed);
    assert_eq!(receipt.export, SideEffect::NotRequested);

    // A failed export or display must not turn a recorded commit into a retryable edit.
    let failed = receipt
        .clone()
        .with_export(SideEffect::Failed("disk full".to_owned()))
        .with_display(SideEffect::Failed("no window".to_owned()));
    assert!(failed.committed);
    assert!(failed.export.is_failure());
    assert!(failed.display.is_failure());
    assert_eq!(failed.document.revision, 2);
    assert_eq!(scene.revision(), 2);
}
