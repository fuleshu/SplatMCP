//! Document identity, revision and snapshot behaviour, at the store's public boundary.
//!
//! These are the acceptance checks task #12 is judged by that do not need the desktop app:
//! one commit and one explicit conflict for two candidates built from the same revision, a
//! slow operation on document A that cannot touch document B, one revision policy across
//! every mutation kind, bounded retention with pins, and explicit failures for stale,
//! expired and foreign handles.

use std::sync::Arc;
use std::thread;

use splatmcp_core::document::{
    ArtifactChecksum, DocumentError, DocumentHandle, Expected, Mutation, RetentionLimits,
    RevisionRecord,
};
use splatmcp_core::{DocumentStore, Splat, SplatPoint, write_ply};

fn splat(points: usize) -> Splat {
    Splat::from_points(
        (0..points)
            .map(|index| {
                SplatPoint::new(
                    [index as f32 * 0.1, 0.0, 0.0],
                    [0.05; 3],
                    [0.5, 0.5, 0.5],
                    0.8,
                    [1.0, 0.0, 0.0, 0.0],
                )
            })
            .collect(),
    )
}

/// A store with a fixed session stamp, so identity text is predictable in assertions.
fn store(limits: RetentionLimits) -> DocumentStore {
    DocumentStore::with_session(limits, 0xabc)
}

#[test]
fn two_candidates_from_one_revision_produce_one_commit_and_one_conflict() {
    let store = store(RetentionLimits::default());
    let opened = store.open(splat(2), Mutation::import("scene.ply"));
    let base = opened.handle;

    // Both candidates were built from the same revision: one of them must lose explicitly.
    let first = store.commit(
        Expected::Handle(base.clone()),
        splat(3),
        Mutation::edit("edit_splat"),
    );
    let second = store.commit(
        Expected::Handle(base.clone()),
        splat(4),
        Mutation::edit("edit_splat"),
    );

    let committed = first.expect("the first candidate commits");
    assert_eq!(committed.handle.revision, 2);
    assert_eq!(committed.point_count, 3);

    let error = second.expect_err("the second candidate must not overwrite");
    assert_eq!(error.code(), "document_conflict");
    assert!(error.is_conflict());
    match error {
        DocumentError::Conflict { expected, current } => {
            assert_eq!(expected.revision, 1);
            assert_eq!(current.revision, 2);
            assert_eq!(current.document_id, base.document_id);
        }
        other => panic!("expected a conflict, got {other:?}"),
    }

    // The loser's geometry was discarded, not merged: the winner is still what is displayed.
    let active = store.active_metadata().unwrap();
    assert_eq!(active.point_count, 3);
    assert_eq!(active.handle.revision, 2);
}

#[test]
fn a_slow_operation_on_the_first_document_cannot_touch_the_second() {
    let store = store(RetentionLimits::default());
    let first = store.open(splat(5), Mutation::open("C:/scenes/first.ply"));

    // A long computation takes a snapshot of document A and works on it outside the lock.
    let snapshot = store.snapshot(Expected::Any).unwrap();
    assert_eq!(snapshot.handle().document_id, first.handle.document_id);

    // Meanwhile the user opens document B.
    let second = store.open(splat(7), Mutation::open("C:/scenes/second.ply"));
    assert_ne!(second.handle.document_id, first.handle.document_id);
    assert_eq!(
        second.handle.revision, 1,
        "a new document starts at revision 1"
    );

    // Document A's snapshot is still readable for its declared lifetime, unchanged.
    assert_eq!(snapshot.len(), 5);
    assert_eq!(snapshot.handle().revision, 1);

    // "Whatever is displayed" resolves at request receipt to document B and says so.
    let resolved = store
        .commit(Expected::Any, splat(9), Mutation::edit("late job"))
        .expect("an unqualified change applies to the displayed document");
    assert_eq!(resolved.handle.document_id, second.handle.document_id);
    assert_eq!(resolved.handle.revision, 2);
    // Document A was not touched by it.
    assert_eq!(store.resolve(&snapshot.handle().clone()).unwrap().len(), 5);

    // Naming document A explicitly is refused, and leaves the display alone.
    let error = store
        .commit(
            Expected::Handle(first.handle.clone()),
            splat(11),
            Mutation::edit("late job"),
        )
        .expect_err("a mutation cannot target a document that is no longer displayed");
    assert_eq!(error.code(), "unknown_document");
    assert_eq!(store.active_metadata().unwrap().point_count, 9);
    assert_eq!(store.resolve(&first.handle).unwrap().len(), 5);
}

#[test]
fn every_mutation_kind_advances_the_revision_by_one() {
    let store = store(RetentionLimits::default());
    let opened = store.open(splat(3), Mutation::open("C:/scenes/house.ply"));
    assert_eq!(opened.handle.revision, 1);
    let handle = opened.handle.clone();

    let mut expected_revision = 1;
    let mut current = handle.clone();
    let steps: Vec<(Mutation, usize)> = vec![
        (Mutation::edit("edit_splat"), 4),
        (
            Mutation::job("job 1", Some("{}".to_owned())).component("roof"),
            5,
        ),
        (Mutation::import("imported.ply"), 6),
        (Mutation::reload("reload"), 7),
    ];
    for (mutation, points) in steps {
        expected_revision += 1;
        let metadata = store
            .commit(Expected::Handle(current.clone()), splat(points), mutation)
            .unwrap();
        assert_eq!(metadata.handle.revision, expected_revision);
        assert_eq!(metadata.point_count, points);
        current = metadata.handle;
    }

    // A component change moves the revision too, and keeps the geometry.
    let metadata = store
        .set_component(Expected::Any, "facade", "set_component")
        .unwrap();
    assert_eq!(metadata.handle.revision, expected_revision + 1);
    assert_eq!(metadata.point_count, 7);
    assert_eq!(
        metadata.provenance.component_id.as_deref(),
        Some("facade"),
        "component metadata is part of the revision's provenance"
    );

    // One policy, one history: every step is recorded, newest first.
    let history: Vec<u64> = metadata
        .history
        .iter()
        .map(|record| record.revision)
        .collect();
    assert_eq!(history.first(), Some(&metadata.handle.revision));
    assert!(
        history.windows(2).all(|pair| pair[0] > pair[1]),
        "{history:?}"
    );
    assert!(metadata.history.len() <= 8);
    assert!(metadata.provenance.created_at_ms <= metadata.provenance.updated_at_ms);

    // Provenance kept what no step overwrote: the source of the geometry.
    assert_eq!(
        metadata.provenance.source_path.as_deref(),
        Some("C:/scenes/house.ply")
    );
    assert_eq!(
        metadata.provenance.last_operation.as_deref(),
        Some("set_component")
    );
}

#[test]
fn exporting_records_provenance_without_advancing_the_revision() {
    let store = store(RetentionLimits::default());
    let opened = store.open(splat(4), Mutation::open("C:/scenes/house.ply"));
    let handle = opened.handle.clone();

    let bytes = write_ply(&splat(4)).unwrap();
    let checksum = ArtifactChecksum::of(&bytes);
    let metadata = store
        .record_export(&handle, "C:/exports/house-v1.ply", checksum, 1_000)
        .unwrap();

    // Same identity, same revision: only provenance changed.
    assert_eq!(metadata.handle, handle);
    assert_eq!(
        metadata.provenance.updated_at_ms,
        opened.provenance.updated_at_ms
    );
    let export = metadata.last_export().expect("the export was recorded");
    assert_eq!(export.path, "C:/exports/house-v1.ply");
    assert_eq!(export.revision, 1);
    assert!(export.checksum.matches(&checksum));
    assert_eq!(export.checksum.algorithm, "fnv1a64");
    assert_eq!(export.checksum.bytes, bytes.len());

    // The artifact checksum identifies those bytes, not the scene: re-encoding is a different
    // artifact, while the document identity is untouched.
    let other_bytes = write_ply(&splat(5)).unwrap();
    assert!(!ArtifactChecksum::of(&other_bytes).matches(&checksum));
    assert_eq!(store.active_handle().unwrap(), handle);

    // The next accepted change still moves the revision by exactly one.
    let advanced = store
        .commit(
            Expected::Handle(handle.clone()),
            splat(6),
            Mutation::edit("edit_splat"),
        )
        .unwrap();
    assert_eq!(advanced.handle.revision, 2);

    // Export records are bounded and newest first.
    for index in 0..6 {
        store
            .record_export(
                &advanced.handle,
                format!("C:/exports/v{index}.ply"),
                checksum,
                2_000 + index,
            )
            .unwrap();
    }
    let metadata = store.metadata_for(&advanced.handle).unwrap();
    assert!(metadata.provenance.exports.len() <= 4);
    assert_eq!(
        metadata.provenance.exports.first().unwrap().path,
        "C:/exports/v5.ply"
    );
}

#[test]
fn reopening_the_same_path_is_a_new_document_and_a_named_revision_is_a_replacement() {
    let store = store(RetentionLimits::default());
    let first = store.open(splat(3), Mutation::open("C:/scenes/house.ply"));
    let second = store.open(splat(3), Mutation::open("C:/scenes/house.ply"));

    assert_ne!(
        first.handle.document_id, second.handle.document_id,
        "a path is provenance, not identity"
    );
    assert_eq!(second.handle.revision, 1);
    assert_eq!(
        first.provenance.file_name, second.provenance.file_name,
        "the name is only a name"
    );

    // An explicit replacement keeps the identity and moves the revision.
    let replacement = store
        .commit(
            Expected::Handle(second.handle.clone()),
            splat(5),
            Mutation::import("house.ply"),
        )
        .unwrap();
    assert_eq!(replacement.handle.document_id, second.handle.document_id);
    assert_eq!(replacement.handle.revision, 2);

    // ...and the same goes for a load that states only the revision it expects.
    let by_revision = store
        .commit(
            Expected::Revision(2),
            splat(6),
            Mutation::import("house.ply"),
        )
        .unwrap();
    assert_eq!(by_revision.handle.document_id, second.handle.document_id);
    assert_eq!(by_revision.handle.revision, 3);
}

#[test]
fn stale_expired_and_foreign_handles_fail_explicitly_and_change_nothing() {
    let store = store(RetentionLimits::new(2, usize::MAX));
    let opened = store.open(splat(3), Mutation::open("C:/scenes/house.ply"));
    let handle = opened.handle.clone();
    let advanced = store
        .commit(
            Expected::Handle(handle.clone()),
            splat(4),
            Mutation::edit("edit_splat"),
        )
        .unwrap();
    assert_eq!(advanced.handle.revision, 2);

    // Stale: the document moved on, so a mutation that quotes revision 1 conflicts.
    let stale = store
        .commit(
            Expected::Handle(handle.clone()),
            splat(9),
            Mutation::edit("edit_splat"),
        )
        .unwrap_err();
    assert_eq!(stale.code(), "document_conflict");

    // Foreign: an identity from another session is unknown, not a conflict.
    let foreign_handle = DocumentHandle::new(splatmcp_core::DocumentId::mint(0xdead, 1), 1);
    let foreign = store
        .commit(
            Expected::Handle(foreign_handle.clone()),
            splat(9),
            Mutation::edit("edit_splat"),
        )
        .unwrap_err();
    assert_eq!(foreign.code(), "unknown_document");
    assert_eq!(
        store.resolve(&foreign_handle).unwrap_err().code(),
        "unknown_document"
    );
    assert_eq!(store.active_handle().unwrap().revision, 2);

    // Tighten retention by moving the document on until revision 1 falls out of the window.
    let mut current = advanced.handle.clone();
    for _ in 0..3 {
        current = store
            .commit(
                Expected::Handle(current),
                splat(4),
                Mutation::edit("edit_splat"),
            )
            .unwrap()
            .handle;
    }
    // Reading an evicted revision is what "expired" means, and it is told apart from a
    // conflict: the revision is gone, not merely behind.
    assert_eq!(
        store.resolve(&handle).unwrap_err().code(),
        "snapshot_expired"
    );

    // The displayed revision is untouched throughout.
    assert_eq!(
        store.resolve(&handle).unwrap_err().code(),
        "snapshot_expired"
    );
    assert_eq!(store.active_handle().unwrap().revision, 5);
    assert_eq!(store.active_metadata().unwrap().point_count, 4);
}

#[test]
fn a_pin_keeps_one_revision_readable_while_the_document_moves_on() {
    let store = store(RetentionLimits::new(1, usize::MAX));
    let opened = store.open(splat(6), Mutation::open("C:/scenes/house.ply"));
    let pinned = store.pin(&opened.handle).unwrap();

    let mut current = opened.handle.clone();
    for _ in 0..4 {
        current = store
            .commit(
                Expected::Handle(current),
                splat(2),
                Mutation::edit("edit_splat"),
            )
            .unwrap()
            .handle;
    }

    // The pinned revision is still exactly what it was, even though retention is over budget.
    let snapshot = store.resolve(&opened.handle).expect("the pin held it");
    assert_eq!(snapshot.len(), 6);
    assert_eq!(snapshot.handle().revision, 1);
    assert!(store.stats().over_budget());
    assert_eq!(store.stats().pins, 1);

    // Two pins on one revision are two tokens: releasing one twice must not drop the
    // protection the other reader is still holding.
    let second = store.pin(&opened.handle).unwrap();
    let copied = second.duplicate();
    assert!(store.release(&pinned));
    assert!(
        !store.release(&pinned),
        "a released pin is not released twice"
    );
    assert_eq!(store.stats().pins, 1, "the other reader keeps its protection");
    assert_eq!(store.resolve(&opened.handle).unwrap().len(), 6);
    assert!(store.release(&second));
    assert!(!store.release(&copied), "a copied token is the same token");
    assert_eq!(store.stats().pins, 0);
    store
        .commit(
            Expected::Handle(current),
            splat(2),
            Mutation::edit("edit_splat"),
        )
        .unwrap();
    assert_eq!(
        store.resolve(&opened.handle).unwrap_err().code(),
        "snapshot_expired"
    );
    assert!(!store.stats().over_budget());
}

#[test]
fn two_threads_racing_on_one_revision_leave_one_winner() {
    let store = Arc::new(store(RetentionLimits::default()));
    let opened = store.open(splat(2), Mutation::import("scene.ply"));
    let base = opened.handle;

    let mut handles = Vec::new();
    for candidate in 0..4 {
        let store = Arc::clone(&store);
        let base = base.clone();
        handles.push(thread::spawn(move || {
            store
                .commit(
                    Expected::Handle(base),
                    splat(candidate + 3),
                    Mutation::edit(format!("edit_splat {candidate}")),
                )
                .map(|metadata| metadata.handle.revision)
                .map_err(|error| error.code().to_owned())
        }));
    }
    let results: Vec<Result<u64, String>> = handles
        .into_iter()
        .map(|handle| handle.join().expect("no thread panics"))
        .collect();

    let winners: Vec<&Result<u64, String>> =
        results.iter().filter(|result| result.is_ok()).collect();
    assert_eq!(winners.len(), 1, "{results:?}");
    assert_eq!(*winners[0].as_ref().unwrap(), 2);
    for loser in results.iter().filter(|result| result.is_err()) {
        assert_eq!(loser.as_ref().unwrap_err(), "document_conflict");
    }
    assert_eq!(store.active_handle().unwrap().revision, 2);
}

#[test]
fn metadata_stays_bounded_for_a_large_document() {
    let store = store(RetentionLimits::default());
    let opened = store.open(splat(200_000), Mutation::open("C:/scenes/big.ply"));
    let metadata = store.metadata_for(&opened.handle).unwrap();

    assert_eq!(metadata.point_count, 200_000);
    assert!(metadata.bounds.is_some());
    assert_eq!(metadata.attributes.len(), 5);
    assert!(metadata.history.len() <= 8);
    assert!(metadata.retained_revisions.len() <= 9);
    assert!(metadata.summary().len() < 200, "{}", metadata.summary());

    // The metadata carries no geometry: it is a fixed-size description either way.
    let small = store.open(splat(2), Mutation::open("C:/scenes/small.ply"));
    let small = store.metadata_for(&small.handle).unwrap();
    assert_eq!(
        std::mem::size_of_val(&metadata) - std::mem::size_of_val(&small),
        0,
        "the description's size does not depend on the document"
    );
}

#[test]
fn a_document_that_is_not_displayed_can_still_be_read_by_handle() {
    let store = store(RetentionLimits::default());
    let first = store.open(splat(3), Mutation::open("C:/scenes/first.ply"));
    // Two edits so the retained revision A@1 is still inside the window.
    let second = store.open(splat(4), Mutation::open("C:/scenes/second.ply"));

    let snapshot = store
        .resolve(&first.handle)
        .expect("a replaced document is readable while it is retained");
    assert_eq!(snapshot.len(), 3);
    assert_eq!(snapshot.handle(), &first.handle);
    assert_eq!(snapshot.provenance().file_name, "first.ply");
    assert!(snapshot.metadata().retained_revisions.contains(&1));

    // Reading does not change which document is displayed.
    assert_eq!(store.active_handle().unwrap(), second.handle);
    assert!(store.metadata_for(&first.handle).is_ok());
}

#[test]
fn history_records_carry_the_operation_that_produced_them() {
    let store = store(RetentionLimits::default());
    let opened = store.open(splat(2), Mutation::open("C:/scenes/house.ply").at_ms(500));
    let advanced = store
        .commit(
            Expected::Handle(opened.handle.clone()),
            splat(3),
            Mutation::job("job 7", Some("{\"recipe\":true}".to_owned()))
                .component("roof")
                .at_ms(900),
        )
        .unwrap();

    let records: Vec<&RevisionRecord> = advanced.history.iter().collect();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].revision, 2);
    assert_eq!(records[0].operation.as_deref(), Some("job 7"));
    assert_eq!(records[0].at_ms, 900);
    assert_eq!(records[1].revision, 1);
    assert_eq!(records[1].at_ms, 500);
    assert!(advanced.provenance.has_recipe());
}
