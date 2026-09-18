//! Compact asset references for bulk operations.
//!
//! A tool call that would otherwise carry geometry inline - a 500 000 gaussian PLY, the
//! buffers of a merge, or the values of one attribute patch - can instead name an
//! *immutable asset* the desktop app already holds. This module is the one place that owns
//! those bytes, so no call has to move a whole scene through JSON to change one attribute.
//!
//! # What makes an asset immutable
//!
//! - A **local file reference** is resolved under the host's file authorization (an
//!   absolute path only), read once and snapshotted. Editing the file after submission
//!   cannot change queued input, because the queued input is no longer the file.
//! - An **inline chunk upload** is the bounded fallback for a caller that cannot reach the
//!   file: declared length and checksum, sequential offsets, resumable status and one
//!   atomic finalize. Bytes are never half-registered.
//! - Every asset carries its kind, media type, schema name, byte length, checksum,
//!   provenance, point count when it can be scanned cheaply, and a lifetime.
//!
//! A path is provenance, never identity. Exactly as [`crate::document`] refuses to treat a
//! file name as a document identity, an asset is addressed by [`AssetId`], and a stale or
//! unknown id fails with [`AssetError::UnknownAsset`] instead of resolving to whatever
//! file happens to sit at the same path now.
//!
//! # Budgets and retention
//!
//! Declared and *expanded* budgets are enforced before allocation: submission size (and the
//! declared length of an upload) against [`AssetBudgets::max_asset_bytes`], the sum of live
//! assets against [`AssetBudgets::max_total_bytes`], and a decoded payload against
//! [`AssetBudgets::max_expanded_points`] / [`AssetBudgets::max_expanded_bytes`]. An
//! over-sized, truncated, mis-hashed or shape-inconsistent payload is refused before any
//! document transaction starts, so a rejected asset never leaves a partial commit.
//!
//! Retention is bounded and documented: an asset expires after
//! [`AssetBudgets::lifetime_ms`], and an abandoned staged upload expires the same way after
//! its last received chunk. Inserting past the count or byte ceiling first sweeps expired
//! entries, then evicts the least recently used ones; a handle already resolved by a caller
//! keeps its bytes alive (eviction only removes the id), so a slow decode still owns a
//! consistent snapshot.
//!
//! # Where this is used
//!
//! - [`patch`] decodes a typed binary attribute patch (declared attribute, dtype, shape,
//!   endian, layout and optional unit conversion) straight onto selected rows.
//! - [`buffers`] is the self-describing container for gaussian arrays, the format a Python
//!   job or a sidecar writes instead of base64 JSON.
//! - [`merge`] turns a PLY or buffer asset into points for a transaction `merge` step.

pub mod buffers;
pub mod merge;
pub mod patch;
mod ply_probe;
mod registry;

#[cfg(test)]
mod tests;

pub use buffers::{
    BUFFERS_MAGIC, BUFFERS_SCHEMA, BufferAttribute, BufferHeader, decode as decode_buffers,
    declared_count as declared_buffer_count, encode as encode_buffers, scan as scan_buffers,
};
pub use merge::{GaussianAsset, decode_points};
pub use patch::{
    AttributePatch, PatchAttribute, PatchDescriptor, PatchDtype, PatchEncoding, PatchEndian,
    PatchError, PatchLayout, PatchReport, PatchShape,
};
pub use registry::{Asset, AssetHandle, AssetRegistry, AssetUpload, UploadProgress, UploadStatus};

use std::fmt;

use crate::document::ArtifactChecksum;

/// Version of the asset contract these types implement.
///
/// Recorded on every [`AssetInfo`] so a caller can tell which schema it is reading, and
/// carried into replies rather than assumed.
pub const ASSET_CONTRACT_VERSION: u32 = 1;

/// Largest single submission accepted by default: 512 MiB.
pub const DEFAULT_MAX_ASSET_BYTES: u64 = 512 * 1024 * 1024;
/// Largest total of live asset bytes kept by default: 1 GiB.
pub const DEFAULT_MAX_TOTAL_BYTES: u64 = 1024 * 1024 * 1024;
/// Most assets kept at once by default.
pub const DEFAULT_MAX_ASSETS: usize = 64;
/// Largest decoded payload accepted by default: the contract's own point ceiling.
pub const DEFAULT_MAX_EXPANDED_POINTS: usize = crate::MAX_POINTS;
/// Largest decoded attribute-patch payload accepted by default: 1 GiB.
pub const DEFAULT_MAX_EXPANDED_BYTES: u64 = 1024 * 1024 * 1024;
/// How long an unused asset stays resolvable by default: 15 minutes.
pub const DEFAULT_LIFETIME_MS: u64 = 15 * 60 * 1000;
/// Largest chunk accepted by one upload append by default: 8 MiB.
pub const DEFAULT_MAX_UPLOAD_CHUNK_BYTES: u64 = 8 * 1024 * 1024;

/// Identity of one asset, minted by an [`AssetRegistry`] for the session that owns it.
///
/// Opaque on purpose: a caller stores it, passes it back and never derives meaning from its
/// text. The rendered form is stable so it can travel in JSON and logs, and it embeds the
/// session stamp so an id from an earlier run of the app fails loudly instead of naming a
/// different asset.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AssetId(String);

impl AssetId {
    /// Mints the `index`-th identity of `session`.
    pub fn mint(session: u64, index: u64) -> Self {
        Self(format!("asset-{session:x}-{index}"))
    }

    /// Reads an identity back from text, or `None` when it was not produced by this format.
    pub fn parse(text: &str) -> Option<Self> {
        let rest = text.strip_prefix("asset-")?;
        let (session, index) = rest.split_once('-')?;
        if session.is_empty() || index.is_empty() {
            return None;
        }
        if !session.chars().all(|character| character.is_ascii_hexdigit())
            || !index.chars().all(|character| character.is_ascii_digit())
        {
            return None;
        }
        Some(Self(text.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AssetId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// What an asset's bytes are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetKind {
    /// A PLY file, decoded by the crate's own reader.
    Ply,
    /// Gaussian arrays in the self-describing [`buffers`] container.
    SplatBuffers,
    /// Raw attribute values, meaningless without a patch descriptor.
    AttributePatch,
}

impl AssetKind {
    /// Every kind, for a capabilities listing.
    pub const ALL: [Self; 3] = [Self::Ply, Self::SplatBuffers, Self::AttributePatch];

    /// Stable name used in replies and requests.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ply => "ply",
            Self::SplatBuffers => "splat_buffers",
            Self::AttributePatch => "attribute_patch",
        }
    }

    /// Media type of the bytes, so a consumer never guesses the encoding.
    pub fn media_type(self) -> &'static str {
        match self {
            Self::Ply => "application/x-ply",
            Self::SplatBuffers => "application/x-splat-buffers",
            Self::AttributePatch => "application/octet-stream",
        }
    }

    /// Schema name of the payload.
    pub fn schema(self) -> &'static str {
        match self {
            Self::Ply => "ply",
            Self::SplatBuffers => buffers::BUFFERS_SCHEMA,
            Self::AttributePatch => "attribute.patch.v1",
        }
    }

    /// Parses the name a request used, or `None` when it is not a kind this app knows.
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "ply" => Some(Self::Ply),
            "splat_buffers" | "buffers" | "splat-buffers" => Some(Self::SplatBuffers),
            "attribute_patch" | "patch" => Some(Self::AttributePatch),
            _ => None,
        }
    }
}

/// Bounded description of one immutable asset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetInfo {
    pub asset_id: AssetId,
    pub kind: AssetKind,
    /// Version of the asset contract this description was written by.
    pub contract_version: u32,
    pub media_type: &'static str,
    pub schema: &'static str,
    /// Bytes held for this asset.
    pub bytes: usize,
    pub checksum: ArtifactChecksum,
    /// Where the bytes came from: an absolute path, or the caller's label.
    pub provenance: String,
    /// Gaussians the payload declares, when its header can be scanned cheaply.
    pub point_count: Option<usize>,
    pub created_at_ms: u64,
    /// When the asset stops being resolvable by id, if a lifetime was set.
    pub expires_at_ms: Option<u64>,
}

impl AssetInfo {
    /// True when the asset is still resolvable at `now`.
    pub fn is_live(&self, now: u64) -> bool {
        self.expires_at_ms.is_none_or(|expires| expires > now)
    }

    /// One bounded line for a reply or a log.
    pub fn summary(&self) -> String {
        let count = self
            .point_count
            .map(|count| format!(", {count} gaussians"))
            .unwrap_or_default();
        format!(
            "{} {} ({} bytes{count}, {})",
            self.asset_id,
            self.kind.as_str(),
            self.bytes,
            self.schema
        )
    }
}

/// Live-asset accounting, so a caller can see the ceiling it is working against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AssetStats {
    pub assets: usize,
    pub bytes: u64,
    /// Staged uploads that have not been finalized.
    pub uploads: usize,
    pub upload_bytes: u64,
    /// Assets dropped because they expired or were evicted.
    pub evicted: u64,
}

/// Budgets one registry enforces, reported verbatim in capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssetBudgets {
    pub max_asset_bytes: u64,
    pub max_total_bytes: u64,
    pub max_assets: usize,
    pub max_expanded_points: usize,
    pub max_expanded_bytes: u64,
    pub lifetime_ms: u64,
    pub max_upload_chunk_bytes: u64,
}

impl Default for AssetBudgets {
    fn default() -> Self {
        Self {
            max_asset_bytes: DEFAULT_MAX_ASSET_BYTES,
            max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
            max_assets: DEFAULT_MAX_ASSETS,
            max_expanded_points: DEFAULT_MAX_EXPANDED_POINTS,
            max_expanded_bytes: DEFAULT_MAX_EXPANDED_BYTES,
            lifetime_ms: DEFAULT_LIFETIME_MS,
            max_upload_chunk_bytes: DEFAULT_MAX_UPLOAD_CHUNK_BYTES,
        }
    }
}

impl AssetBudgets {
    /// Checks a declared submission size before any allocation happens.
    pub fn check_declared(&self, bytes: u64, what: &'static str) -> Result<(), AssetError> {
        if bytes > self.max_asset_bytes {
            return Err(AssetError::TooLarge {
                what,
                requested: bytes,
                limit: self.max_asset_bytes,
            });
        }
        Ok(())
    }

    /// Checks a decoded payload before the decode allocates.
    pub fn check_expanded(
        &self,
        points: usize,
        bytes: u64,
        what: &'static str,
    ) -> Result<(), AssetError> {
        if points > self.max_expanded_points {
            return Err(AssetError::TooLarge {
                what,
                requested: points as u64,
                limit: self.max_expanded_points as u64,
            });
        }
        if bytes > self.max_expanded_bytes {
            return Err(AssetError::TooLarge {
                what,
                requested: bytes,
                limit: self.max_expanded_bytes,
            });
        }
        Ok(())
    }

    /// The exact numbers, for the capabilities reply.
    pub fn describe(&self) -> String {
        format!(
            "asset_bytes<={}, total_bytes<={}, assets<={}, expanded_points<={}, \
             expanded_bytes<={}, lifetime_ms<={}, upload_chunk_bytes<={}",
            self.max_asset_bytes,
            self.max_total_bytes,
            self.max_assets,
            self.max_expanded_points,
            self.max_expanded_bytes,
            self.lifetime_ms,
            self.max_upload_chunk_bytes
        )
    }
}

/// Everything that can go wrong while registering, resolving or decoding an asset.
#[derive(Debug, Clone, PartialEq)]
pub enum AssetError {
    /// The asset id was never minted by this session, or has been released.
    UnknownAsset { asset_id: AssetId },
    /// The asset existed but its lifetime has passed.
    Expired { asset_id: AssetId },
    /// The file could not be read, with the reason the filesystem gave.
    Io { path: String, reason: String },
    /// A file reference must be absolute: a relative path would resolve against the
    /// process working directory, which is not what the caller meant.
    RelativePath { path: String },
    /// The payload, or its declared length, is above a budget.
    TooLarge {
        what: &'static str,
        requested: u64,
        limit: u64,
    },
    /// The registry ceiling would be exceeded and nothing could be evicted.
    RegistryFull { limit: usize },
    /// The bytes do not match the declared checksum.
    HashMismatch { expected: u64, actual: u64 },
    /// Fewer bytes arrived than were declared.
    Truncated { declared: u64, received: u64 },
    /// No staged upload has that id.
    UploadUnknown { upload_id: u64 },
    /// An append did not continue where the previous one stopped.
    UploadOffset { expected: u64, given: u64 },
    /// The upload already finished (or was cancelled), so it cannot be appended to.
    UploadClosed { upload_id: u64 },
    /// A chunk was larger than one append may carry.
    UploadChunkTooLarge { requested: u64, limit: u64 },
    /// The kind name is not one this app knows.
    UnknownKind { kind: String },
    /// The payload's own header or manifest is wrong.
    Malformed { reason: String },
    /// A decoded gaussian violated the contract, with its row and the reason.
    InvalidPoint { row: usize, reason: String },
    /// An attribute patch could not be planned or applied.
    Patch(PatchError),
    /// The registry lock is poisoned, so nothing can be read or written safely.
    Unavailable { reason: String },
}

impl fmt::Display for AssetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownAsset { asset_id } => write!(
                formatter,
                "asset {asset_id} is not known to this app; register it again and retry"
            ),
            Self::Expired { asset_id } => {
                write!(formatter, "asset {asset_id} has expired; register it again")
            }
            Self::Io { path, reason } => write!(formatter, "could not read {path}: {reason}"),
            Self::RelativePath { path } => write!(
                formatter,
                "'{path}' is not an absolute path; pass the full path of the file"
            ),
            Self::TooLarge {
                what,
                requested,
                limit,
            } => write!(
                formatter,
                "{what} is {requested}, above the {limit} limit; split the work or raise the budget"
            ),
            Self::RegistryFull { limit } => write!(
                formatter,
                "the asset registry holds its {limit} entries and none can be evicted; release an asset first"
            ),
            Self::HashMismatch { expected, actual } => write!(
                formatter,
                "checksum mismatch: declared {expected:016x} but the bytes hash to {actual:016x}"
            ),
            Self::Truncated { declared, received } => write!(
                formatter,
                "the payload is truncated: {declared} bytes declared but {received} received"
            ),
            Self::UploadUnknown { upload_id } => write!(
                formatter,
                "upload {upload_id} is not staged (finished, cancelled or expired)"
            ),
            Self::UploadOffset { expected, given } => write!(
                formatter,
                "chunk offset {given} does not continue the upload; the next offset is {expected}"
            ),
            Self::UploadClosed { upload_id } => {
                write!(formatter, "upload {upload_id} is already finished")
            }
            Self::UploadChunkTooLarge { requested, limit } => write!(
                formatter,
                "chunk of {requested} bytes is above the {limit} byte append limit"
            ),
            Self::UnknownKind { kind } => write!(
                formatter,
                "unknown asset kind '{kind}'; use ply, splat_buffers or attribute_patch"
            ),
            Self::Malformed { reason } => write!(formatter, "the payload is malformed: {reason}"),
            Self::InvalidPoint { row, reason } => {
                write!(formatter, "row {row} is not a valid gaussian: {reason}")
            }
            Self::Patch(error) => write!(formatter, "{error}"),
            Self::Unavailable { reason } => {
                write!(formatter, "the asset registry is unavailable: {reason}")
            }
        }
    }
}

impl std::error::Error for AssetError {}

impl From<PatchError> for AssetError {
    fn from(error: PatchError) -> Self {
        Self::Patch(error)
    }
}

impl AssetError {
    /// Stable machine readable code, for structured replies.
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownAsset { .. } => "unknown_asset",
            Self::Expired { .. } => "asset_expired",
            Self::Io { .. } => "asset_io",
            Self::RelativePath { .. } => "asset_relative_path",
            Self::TooLarge { .. } => "budget_exceeded",
            Self::RegistryFull { .. } => "asset_registry_full",
            Self::HashMismatch { .. } => "checksum_mismatch",
            Self::Truncated { .. } => "truncated_payload",
            Self::UploadUnknown { .. } => "unknown_upload",
            Self::UploadOffset { .. } => "upload_offset_mismatch",
            Self::UploadClosed { .. } => "upload_closed",
            Self::UploadChunkTooLarge { .. } => "upload_chunk_too_large",
            Self::UnknownKind { .. } => "unknown_asset_kind",
            Self::Malformed { .. } => "malformed_payload",
            Self::InvalidPoint { .. } => "invalid_gaussian",
            Self::Patch(error) => error.code(),
            Self::Unavailable { .. } => "asset_unavailable",
        }
    }
}

/// Checksum of `bytes`, named the way every asset reply names it.
pub(crate) fn checksum_of(bytes: &[u8]) -> ArtifactChecksum {
    ArtifactChecksum::of(bytes)
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn an_asset_id_round_trips_and_rejects_foreign_text() {
        let id = AssetId::mint(0x4f2a, 3);
        assert_eq!(id.as_str(), "asset-4f2a-3");
        assert_eq!(AssetId::parse(id.as_str()), Some(id));
        assert_eq!(AssetId::parse("asset-3"), None);
        assert_eq!(AssetId::parse("doc-4f2a-3"), None);
        assert_eq!(AssetId::parse("C:/scenes/house.ply"), None);
    }

    #[test]
    fn kinds_round_trip_and_carry_their_media_type() {
        for kind in AssetKind::ALL {
            assert_eq!(AssetKind::parse(kind.as_str()), Some(kind));
            assert!(kind.media_type().starts_with("application/"));
            assert!(!kind.schema().is_empty());
        }
        assert_eq!(AssetKind::parse("buffers"), Some(AssetKind::SplatBuffers));
        assert_eq!(AssetKind::parse("csv"), None);
    }

    #[test]
    fn budgets_describe_themselves_and_refuse_what_is_too_large() {
        let budgets = AssetBudgets {
            max_asset_bytes: 16,
            max_expanded_points: 4,
            max_expanded_bytes: 64,
            ..AssetBudgets::default()
        };
        assert!(budgets.check_declared(16, "asset").is_ok());
        let error = budgets.check_declared(17, "asset").unwrap_err();
        assert_eq!(error.code(), "budget_exceeded");
        assert!(error.to_string().contains("above the 16 limit"));
        assert_eq!(
            budgets.check_expanded(5, 64, "patch").unwrap_err().code(),
            "budget_exceeded"
        );
        assert_eq!(
            budgets.check_expanded(4, 65, "patch").unwrap_err().code(),
            "budget_exceeded"
        );
        assert!(budgets.describe().contains("asset_bytes<=16"));
        assert!(AssetBudgets::default().describe().contains("lifetime_ms<="));
    }

    #[test]
    fn an_info_reports_when_it_is_live() {
        let info = AssetInfo {
            asset_id: AssetId::mint(1, 1),
            kind: AssetKind::Ply,
            contract_version: ASSET_CONTRACT_VERSION,
            media_type: AssetKind::Ply.media_type(),
            schema: AssetKind::Ply.schema(),
            bytes: 1024,
            checksum: ArtifactChecksum::of(b"x"),
            provenance: "C:/scenes/house.ply".to_owned(),
            point_count: Some(500_000),
            created_at_ms: 10,
            expires_at_ms: Some(20),
        };
        assert!(info.is_live(19));
        assert!(!info.is_live(20));
        assert!(info.summary().contains("500000 gaussians"));
        let forever = AssetInfo {
            expires_at_ms: None,
            ..info
        };
        assert!(forever.is_live(u64::MAX));
    }
}
