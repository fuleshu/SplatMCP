//! Acceptance tests for compact asset references.
//!
//! These exercise the whole path an MCP call takes: a file or a chunked upload becomes an
//! asset, the asset is decoded under budget, and the result is committed through the one
//! transaction service. What is asserted here is exactly what the task asks to prove:
//! nothing per point travels through the request, a stale source file cannot change queued
//! input, and every corrupt, truncated, mis-hashed, over-budget or stale-revision case fails
//! **before** the document advances.

use std::path::PathBuf;
use std::sync::Arc;

use super::patch::{AttributePatch, PatchAttribute, PatchDescriptor, PatchShape};
use super::registry::{AssetRegistry, AssetUpload};
use super::{AssetBudgets, AssetKind, decode_points, encode_buffers};
use crate::document::{DocumentStore, Expected, Mutation, RetentionLimits};
use crate::ply::write_ply;
use crate::splat::SplatPoint;
use crate::transaction::{
    BatchStep, BatchTargets, EditBatch, TransactionLimits, TransactionService,
};
use crate::{EditOp, Selection};

/// One scratch directory per test, removed by [`scratch`]'s caller.
fn scratch(name: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "splatmcp-assets-{}-{name}-{}",
        std::process::id(),
        crate::document::now_ms()
    ));
    std::fs::create_dir_all(&directory).expect("a writable scratch directory");
    directory
}

fn store_with(points: Vec<SplatPoint>) -> Arc<DocumentStore> {
    let store = DocumentStore::with_session(RetentionLimits::default(), 0x51e);
    store.open(
        crate::Splat::from_points(points),
        Mutation::import("base.ply"),
    );
    Arc::new(store)
}

fn service(store: &Arc<DocumentStore>) -> TransactionService {
    TransactionService::new(Arc::clone(store), TransactionLimits::default())
}

/// Gaussians laid out along +X, so a selection by index is also a selection by position.
fn line(count: usize) -> Vec<SplatPoint> {
    (0..count)
        .map(|index| {
            SplatPoint::new(
                [index as f32, 0.0, 0.0],
                [0.01, 0.01, 0.01],
                [0.5, 0.5, 0.5],
                0.8,
                [1.0, 0.0, 0.0, 0.0],
            )
        })
        .collect()
}

#[test]
fn a_merge_from_a_file_asset_commits_without_inline_points() {
    let directory = scratch("merge");
    let path = directory.join("second.ply");
    let second = crate::Splat::from_points(line(5).into_iter().map(|point| SplatPoint::new(
        [point.position[0], 2.0, 0.0],
        point.scale,
        point.color,
        point.opacity,
        point.rotation,
    )).collect());
    std::fs::write(&path, write_ply(&second).unwrap()).unwrap();

    let registry = AssetRegistry::default();
    let info = registry.register_file(AssetKind::Ply, &path).unwrap();
    assert_eq!(info.point_count, Some(5));
    // The compact request is the id plus the descriptor: no geometry travels with it.
    assert!(info.asset_id.as_str().len() < 24);

    let store = store_with(line(3));
    let service = service(&store);
    let handle = registry.resolve(&info.asset_id).unwrap();
    let source = decode_points(&handle, &registry.budgets()).unwrap();
    assert_eq!(source.len(), 5);
    assert!(source.describe().contains("5 gaussian(s)"));
    assert!(!source.describe().contains("0.0"), "a receipt never carries geometry");

    let batch = EditBatch::new(vec![BatchStep::new(EditOp::Merge {
        points: source.points.clone(),
    })])
    .with_operation_id("merge-1");
    let receipt = service
        .commit(Expected::Any, &batch, Mutation::edit("merge_asset"))
        .unwrap();
    assert_eq!(receipt.point_count, 8);
    assert_eq!(store.snapshot(Expected::Any).unwrap().splat().len(), 8);

    // The merge is the same retry-safe commit as an inline one.
    let replay = service
        .commit(Expected::Any, &batch, Mutation::edit("merge_asset"))
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(store.snapshot(Expected::Any).unwrap().splat().len(), 8);
    std::fs::remove_dir_all(&directory).ok();
}

#[test]
fn a_large_asset_merge_is_bounded_metadata_and_one_commit() {
    // 200 000 gaussians on disk, decoded and merged from an id: the point of the asset path
    // is that the request stays this small however large the scene is.
    let directory = scratch("large");
    let path = directory.join("large.ply");
    let large = crate::Splat::from_points(line(200_000));
    let bytes = write_ply(&large).unwrap();
    std::fs::write(&path, &bytes).unwrap();

    let registry = AssetRegistry::default();
    let info = registry.register_file(AssetKind::Ply, &path).unwrap();
    assert_eq!(info.point_count, Some(200_000));

    let store = store_with(line(1_000));
    let service = service(&store);
    let handle = registry.resolve(&info.asset_id).unwrap();
    let source = decode_points(&handle, &registry.budgets()).unwrap();
    assert_eq!(source.len(), 200_000);

    let batch = EditBatch::new(vec![BatchStep::new(EditOp::Merge {
        points: source.points,
    })])
    .with_operation_id("merge-large");
    let receipt = service
        .commit(Expected::Any, &batch, Mutation::edit("merge_asset"))
        .unwrap();
    assert_eq!(receipt.point_count, 201_000);
    // What a caller gets back is identity, counts and the artifact reference.
    let reply = format!(
        "{} {} {}",
        receipt.recorded.document_id, receipt.recorded.revision, receipt.point_count
    );
    assert!(reply.len() < 80, "{reply}");
    std::fs::remove_dir_all(&directory).ok();
}

#[test]
fn patching_one_attribute_does_not_retransmit_the_scene() {
    let store = store_with(line(5_000));
    let service = service(&store);
    let registry = AssetRegistry::default();

    // The payload covers two gaussians and one attribute: 24 bytes, not 320 000.
    let payload: Vec<u8> = [0.25f32, 0.25, 0.25, 0.75, 0.75, 0.75]
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    let info = registry
        .register_bytes(AssetKind::AttributePatch, payload.clone(), "hair colour")
        .unwrap();
    let handle = registry.resolve(&info.asset_id).unwrap();
    let patch = AttributePatch::plan(
        handle.bytes(),
        format!("asset {}", info.asset_id),
        PatchDescriptor {
            shape: PatchShape::with_rows(PatchAttribute::Color, 2),
            ..PatchDescriptor::scalar(PatchAttribute::Color)
        },
        &registry.budgets(),
    )
    .unwrap();
    assert_eq!(patch.rows(), 2);
    assert_eq!(patch.source_bytes(), 24, "only the patched values travel");
    assert_eq!(patch.decoded_bytes(), 24);

    let batch = EditBatch::new(vec![BatchStep::with_targets(
        EditOp::Patch {
            patch: Arc::new(patch),
        },
        BatchTargets {
            selection: Selection {
                first: Some(2),
                ..Selection::default()
            },
            ..BatchTargets::all()
        },
    )])
    .with_operation_id("patch-1");
    let receipt = service
        .commit(Expected::Any, &batch, Mutation::edit("patch_attribute"))
        .unwrap();
    assert_eq!(receipt.point_count, 5_000, "nothing was added or lost");

    let splat = store.snapshot(Expected::Any).unwrap();
    let points = &splat.splat().points;
    assert_eq!(points[0].color, [0.25, 0.25, 0.25]);
    assert_eq!(points[1].color, [0.75, 0.75, 0.75]);
    assert_eq!(points[0].position, [0.0, 0.0, 0.0]);
    assert_eq!(points[0].scale, [0.01; 3]);
    assert_eq!(points[0].opacity, 0.8);
    assert_eq!(points[2].color, [0.5; 3], "unselected gaussians are untouched");
}

#[test]
fn a_file_changed_after_registration_cannot_change_what_is_committed() {
    let directory = scratch("snapshot");
    let path = directory.join("second.ply");
    std::fs::write(&path, write_ply(&crate::Splat::from_points(line(2))).unwrap()).unwrap();

    let registry = AssetRegistry::default();
    let info = registry.register_file(AssetKind::Ply, &path).unwrap();
    // The work is queued: the bytes are already snapshotted.
    let handle = registry.resolve(&info.asset_id).unwrap();

    // The source file is replaced with a completely different scene before the merge runs.
    std::fs::write(
        &path,
        write_ply(&crate::Splat::from_points(line(9))).unwrap(),
    )
    .unwrap();

    let store = store_with(line(1));
    let service = service(&store);
    let source = decode_points(&handle, &registry.budgets()).unwrap();
    assert_eq!(source.len(), 2, "the snapshot, not the file, is decoded");
    let batch = EditBatch::new(vec![BatchStep::new(EditOp::Merge {
        points: source.points,
    })])
    .with_operation_id("merge-snapshot");
    let receipt = service
        .commit(Expected::Any, &batch, Mutation::edit("merge_asset"))
        .unwrap();
    assert_eq!(receipt.point_count, 3);
    std::fs::remove_dir_all(&directory).ok();
}

#[test]
fn a_chunked_upload_becomes_the_same_snapshot_and_survives_verification() {
    let registry = AssetRegistry::default();
    let bytes = write_ply(&crate::Splat::from_points(line(4))).unwrap();
    let checksum = super::checksum_of(&bytes);
    let (_, status) = AssetUpload::begin(
        &registry,
        AssetKind::Ply,
        bytes.len() as u64,
        Some(checksum.value),
        "chunked merge",
    )
    .unwrap();

    let step = 64 * 1024;
    let mut offset = 0u64;
    while (offset as usize) < bytes.len() {
        let end = (offset as usize + step).min(bytes.len());
        let status = registry
            .upload_append(status.upload_id, offset, &bytes[offset as usize..end])
            .unwrap();
        offset = status.next_offset;
    }
    let info = registry.upload_finalize(status.upload_id).unwrap();
    assert_eq!(info.point_count, Some(4));
    assert!(info.checksum.matches(&checksum));

    let handle = registry.resolve(&info.asset_id).unwrap();
    let decoded = decode_points(&handle, &AssetBudgets::default()).unwrap();
    assert_eq!(decoded.len(), 4);

    // The same bytes in a buffer container decode identically.
    let buffers = encode_buffers(&decoded.points, false);
    let from_buffers = decode_points(
        &registry
            .register_bytes(AssetKind::SplatBuffers, buffers, "buffers")
            .map(|info| registry.resolve(&info.asset_id).unwrap())
            .unwrap(),
        &AssetBudgets::default(),
    )
    .unwrap();
    assert_eq!(from_buffers.len(), 4);
    assert_eq!(from_buffers.points[1].position, decoded.points[1].position);
}

#[test]
fn every_bad_payload_fails_before_the_document_advances() {
    let directory = scratch("failures");
    let store = store_with(line(3));
    let service = service(&store);
    let registry = AssetRegistry::default();
    let revision = store.snapshot(Expected::Any).unwrap().handle().revision;

    // A corrupt PLY, a truncated file, a wrong declared hash and an over-budget payload are
    // all refused while registering: nothing to commit, no revision advanced.
    let corrupt = directory.join("corrupt.ply");
    std::fs::write(&corrupt, b"not a ply").unwrap();
    assert_eq!(
        registry
            .register_file(AssetKind::Ply, &corrupt)
            .unwrap_err()
            .code(),
        "malformed_payload"
    );

    let cut = directory.join("cut.ply");
    let full = write_ply(&crate::Splat::from_points(line(4))).unwrap();
    // Keep the whole header and a fragment of the body: the header declares more gaussians
    // than the file carries, which is exactly the truncation the probe catches.
    let marker = full
        .windows(10)
        .position(|window| window == b"end_header")
        .expect("a PLY header");
    let header_end = full[marker + 10..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map(|offset| marker + 10 + offset + 1)
        .expect("a terminated header");
    let cut_bytes = full[..header_end + 8].to_vec();
    std::fs::write(&cut, &cut_bytes).unwrap();
    let error = registry.register_file(AssetKind::Ply, &cut).unwrap_err();
    assert_eq!(error.code(), "malformed_payload");
    assert!(error.to_string().contains("truncated"), "{error}");

    let good = directory.join("good.ply");
    let bytes = write_ply(&crate::Splat::from_points(line(4))).unwrap();
    std::fs::write(&good, &bytes).unwrap();
    assert_eq!(
        registry
            .register_file_with(AssetKind::Ply, &good, Some(0x1234))
            .unwrap_err()
            .code(),
        "checksum_mismatch"
    );

    // A payload that does not match its declared shape is refused while planning.
    let tiny = registry
        .register_bytes(AssetKind::AttributePatch, vec![0u8; 4], "tiny")
        .unwrap();
    let tiny = registry.resolve(&tiny.asset_id).unwrap();
    let error = AttributePatch::plan(
        tiny.bytes(),
        "tiny",
        PatchDescriptor {
            shape: PatchShape::with_rows(PatchAttribute::Color, 1),
            ..PatchDescriptor::scalar(PatchAttribute::Color)
        },
        &registry.budgets(),
    )
    .unwrap_err();
    assert_eq!(error.code(), "payload_length_mismatch");

    // A patch whose payload is larger than the budget is refused while planning.
    let handle = registry
        .register_bytes(AssetKind::AttributePatch, vec![0u8; 12], "values")
        .map(|info| registry.resolve(&info.asset_id).unwrap())
        .unwrap();
    let error = AttributePatch::plan(
        handle.bytes(),
        "values",
        PatchDescriptor::scalar(PatchAttribute::Position),
        &AssetBudgets {
            max_expanded_points: 0,
            ..AssetBudgets::default()
        },
    )
    .unwrap_err();
    assert_eq!(error.code(), "budget_exceeded");

    // A patch that does not match the selection is refused by the transaction, and the
    // document keeps its revision.
    let patch = AttributePatch::plan(
        handle.bytes(),
        "values",
        PatchDescriptor::scalar(PatchAttribute::Position),
        &AssetBudgets::default(),
    )
    .unwrap();
    let batch = EditBatch::new(vec![BatchStep::with_targets(
        EditOp::Patch {
            patch: Arc::new(patch),
        },
        BatchTargets {
            selection: Selection {
                first: Some(2),
                ..Selection::default()
            },
            ..BatchTargets::all()
        },
    )]);
    let error = service
        .commit(Expected::Any, &batch, Mutation::edit("patch"))
        .unwrap_err();
    assert!(
        error.to_string().contains("row_count_mismatch"),
        "{error}"
    );
    assert_eq!(
        store.snapshot(Expected::Any).unwrap().handle().revision,
        revision
    );

    // A stale expected revision is refused too, before any candidate is built.
    let stale = Expected::Revision(revision + 7);
    let merge = EditBatch::new(vec![BatchStep::new(EditOp::Merge {
        points: line(1),
    })]);
    let error = service
        .commit(stale, &merge, Mutation::edit("merge"))
        .unwrap_err();
    assert_eq!(error.code(), "document_conflict");
    assert_eq!(
        store.snapshot(Expected::Any).unwrap().handle().revision,
        revision
    );
    std::fs::remove_dir_all(&directory).ok();
}
