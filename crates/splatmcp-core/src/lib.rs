//! SplatMCP core: an in-memory 3D Gaussian Splat model plus PLY import and export.
//!
//! The conventions this crate implements - axes, units, quaternion order, colour space and
//! the activated-versus-serialized distinction - are stated once in [`contract`] and are
//! versioned there.
//!
//! Colour is always a fixed RGB value - spherical harmonics bands above degree 0
//! are never stored, so every file this crate writes is SH degree 0.
//!
//! Units are deliberately chosen to be meaningful to a caller writing JSON rather
//! than file-native:
//! - `position` is world space metres
//! - `scale` is the *activated* ellipsoid radius in metres (PLY stores `ln(scale)`)
//! - `color` is linear RGB in `0..=1` (PLY stores `(color - 0.5) / SH_C0`)
//! - `opacity` is `0..=1` (PLY stores the sigmoid logit)
//! - `rotation` is a unit quaternion `(w, x, y, z)` (PLY order `rot_0..rot_3`)

mod authoring;
pub mod asset;
pub mod components;
pub mod contract;
pub mod document;
mod edit;
pub mod fixtures;
pub mod inspection;
pub mod job;
mod ply;
pub mod publish;
mod splat;
pub mod transaction;
pub mod validation;

pub use authoring::{MAX_POINTS, Rng, Shape, SplatParams, build, splat_from_points};
pub use asset::{
    ASSET_CONTRACT_VERSION, Asset, AssetBudgets, AssetError, AssetHandle, AssetId, AssetInfo,
    AssetKind, AssetRegistry, AssetStats, AssetUpload, AttributePatch, PatchAttribute,
    PatchDescriptor, PatchDtype, PatchEncoding, PatchEndian, PatchError, PatchLayout, PatchReport,
    PatchShape, UploadProgress, UploadStatus, decode_points,
};
pub use components::{
    AuthoringSet, Component, ComponentId, Frame, ImportSummary, LocalTransform, PointId,
    SelectionError, SelectionHandle, SelectionHandles, SelectionQuery, Sphere,
};
pub use document::{
    ArtifactChecksum, DocumentError, DocumentHandle, DocumentId, DocumentMetadata, DocumentStore,
    Expected, ExportRecord, Mutation, MutationKind, Provenance, RetentionLimits, RetentionStats,
    RevisionRecord, Snapshot, SnapshotPin,
};
pub use edit::{Box3, EditOp, EditStep, OpReport, Selection, apply, apply_all};
pub use inspection::{Distribution, InspectionReport, OwnedBuffers};
pub use job::{
    JOB_CONTRACT_VERSION, JobAdmission, JobBody, JobContext, JobCounts, JobError, JobFailure, JobId,
    JobKind, JobLimits, JobLogEntry, JobPhase, JobProgress, JobReceipt, JobRequest, JobResult,
    JobService, JobState, JobStats, JobView, LogLevel, SideEffectState, checksum_hex,
};
pub use ply::{
    PlyImportPolicy, PlyReport, read_ply, read_ply_repairing, read_ply_with_policy, write_ply,
};
pub use publish::{
    ACK_TIMEOUT_MS, DocumentPublication, PUBLICATION_CONTRACT_VERSION, PublicationError,
    PublicationNotice, PublicationNoticeOutcome, PublicationOutcome, PublicationRequest,
    PublicationSource, PublicationStatus, PublicationTracker, RendererCapabilities,
};
pub use splat::{Bounds, Splat, SplatPoint, SplatStats};
pub use transaction::{
    BatchStep, BatchStepReport, BatchTargets, ComponentChange, ComponentList, EditBatch,
    HistoryEntry, HistoryReport, PreviewCommit, PreviewOutcome, PreviewReport, PreviewSnapshot,
    ReceiptDocument, ReceiptSlot, SideEffect, TargetResolution, TransactionError,
    TransactionLimits, TransactionReceipt, TransactionService,
};
pub use validation::{
    MAX_REPORTED_ISSUES, ValidationError, ValidationIssue, ValidationLimits, ValidationReason,
    ValidationReport,
};

pub(crate) use splat::normalize_quat;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum SplatError {
    #[error("{0}")]
    Format(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// One or more gaussians failed the contract, with the located reasons.
    #[error("{0}")]
    Invalid(#[from] ValidationError),
}

pub type Result<T> = std::result::Result<T, SplatError>;

/// SH band-0 constant.
pub const SH_C0: f32 = 0.28209479177387814;

/// Converts an RGB channel in `0..=1` to the PLY `f_dc_*` SH DC coefficient.
#[inline]
pub fn color_to_dc(color: f32) -> f32 {
    (color - 0.5) / SH_C0
}

/// Converts a PLY `f_dc_*` SH DC coefficient to an RGB channel in `0..=1`.
#[inline]
pub fn dc_to_color(dc: f32) -> f32 {
    (SH_C0 * dc + 0.5).clamp(0.0, 1.0)
}

/// Logistic sigmoid, the 3DGS activation used for opacity.
#[inline]
pub fn sigmoid(value: f32) -> f32 {
    1.0 / (1.0 + (-value).exp())
}

/// Inverse sigmoid with the endpoints pulled in so saturated values stay finite.
#[inline]
pub fn inv_sigmoid(value: f32) -> f32 {
    let clamped = value.clamp(0.5 / 255.0, 1.0 - 0.5 / 255.0);
    (clamped / (1.0 - clamped)).ln()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opacity_and_colour_conversions_invert() {
        for value in [0.02_f32, 0.5, 0.98] {
            assert!((sigmoid(inv_sigmoid(value)) - value).abs() < 1e-6);
        }
        for value in [0.0_f32, 0.25, 1.0] {
            assert!((dc_to_color(color_to_dc(value)) - value).abs() < 1e-6);
        }
    }
}
