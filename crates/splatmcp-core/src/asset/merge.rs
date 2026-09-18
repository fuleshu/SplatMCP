//! Turning a gaussian asset into points a transaction can merge.
//!
//! A `merge` step needs points, but a caller must not have to send them: naming a PLY asset
//! (or a buffer asset) is enough. This module decodes that asset **once**, under the
//! registry's budgets, and hands the points to the existing transaction path, so an
//! asset-based merge is the same atomic, retry-safe, undoable commit as an inline one - not
//! a second append mechanism.
//!
//! The declared point count in the asset header bounds the decode before it starts, and the
//! decoded count is checked again afterwards, so a header that lies about its size cannot
//! turn into an unbounded allocation.

use crate::asset::registry::AssetHandle;
use crate::asset::{AssetBudgets, AssetError, AssetKind, buffers};
use crate::document::ArtifactChecksum;
use crate::ply::{PlyImportPolicy, read_ply_with_policy};
use crate::splat::SplatPoint;

/// Bytes one decoded gaussian occupies: five `f32` attributes in the canonical model.
const DECODED_BYTES_PER_POINT: u64 = 16 * 4;

/// Gaussians decoded from one asset, with the identity of the bytes they came from.
#[derive(Debug, Clone, PartialEq)]
pub struct GaussianAsset {
    pub points: Vec<SplatPoint>,
    /// Where the bytes came from, for the receipt.
    pub label: String,
    /// Kind of the payload that was decoded.
    pub kind: AssetKind,
    /// Checksum of the submitted bytes, not of the decoded values.
    pub checksum: ArtifactChecksum,
}

impl GaussianAsset {
    pub fn len(&self) -> usize {
        self.points.len()
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    /// One bounded line for a receipt: never the geometry.
    pub fn describe(&self) -> String {
        format!(
            "{} gaussian(s) from {} ({} {}, {})",
            self.points.len(),
            self.label,
            self.checksum.bytes,
            "bytes",
            self.checksum.hex()
        )
    }
}

/// Decodes the gaussians an asset holds.
///
/// PLY assets are read strictly: a file that would need repair is refused here with its
/// indexed reason, exactly as the desktop's own open path refuses it, because a merge
/// silently repairing geometry would change data nobody asked to change.
pub fn decode_points(asset: &AssetHandle, budgets: &AssetBudgets) -> Result<GaussianAsset, AssetError> {
    if let Some(declared) = asset.point_count() {
        budgets.check_expanded(
            declared,
            declared as u64 * DECODED_BYTES_PER_POINT,
            "merge source",
        )?;
    }
    let decoded = decode_payload(
        asset.kind(),
        asset.bytes(),
        asset.source(),
        &asset.checksum(),
        budgets,
    )?;
    Ok(decoded)
}

/// Decodes points from bytes a caller already has, without a registry entry.
///
/// Used for the inline path (a small merge) and by tests; the budget rules are identical.
pub fn decode_payload(
    kind: AssetKind,
    bytes: &[u8],
    label: &str,
    checksum: &ArtifactChecksum,
    budgets: &AssetBudgets,
) -> Result<GaussianAsset, AssetError> {
    let points = match kind {
        AssetKind::Ply => {
            let (splat, _report) = read_ply_with_policy(bytes, PlyImportPolicy::Strict)
                .map_err(|error| AssetError::Malformed {
                    reason: format!("{label} is not a readable PLY: {error}"),
                })?;
            splat.points
        }
        AssetKind::SplatBuffers => buffers::decode(bytes, budgets)?,
        AssetKind::AttributePatch => {
            return Err(AssetError::UnknownKind {
                kind: "attribute_patch as a merge source".to_owned(),
            });
        }
    };
    if points.len() as u64 > budgets.max_expanded_points as u64 {
        return Err(AssetError::TooLarge {
            what: "the decoded merge source",
            requested: points.len() as u64,
            limit: budgets.max_expanded_points as u64,
        });
    }
    Ok(GaussianAsset {
        points,
        label: label.to_owned(),
        kind,
        checksum: *checksum,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset::{AssetRegistry, encode_buffers};

    #[test]
    fn a_ply_asset_decodes_into_points_that_match_the_file() {
        let splat = crate::fixtures::axis_fixture();
        let bytes = crate::ply::write_ply(&splat).unwrap();
        let checksum = ArtifactChecksum::of(&bytes);
        let decoded = decode_payload(
            AssetKind::Ply,
            &bytes,
            "scene.ply",
            &checksum,
            &AssetBudgets::default(),
        )
        .unwrap();
        assert_eq!(decoded.len(), splat.len());
        assert_eq!(decoded.points[0].position, splat.points[0].position);
        assert!(decoded.describe().contains("scene.ply"));
    }

    #[test]
    fn a_truncated_ply_is_refused_rather_than_repaired() {
        let splat = crate::fixtures::axis_fixture();
        let mut bytes = crate::ply::write_ply(&splat).unwrap();
        bytes.truncate(bytes.len() / 2);
        let checksum = ArtifactChecksum::of(&bytes);
        let error = decode_payload(
            AssetKind::Ply,
            &bytes,
            "cut.ply",
            &checksum,
            &AssetBudgets::default(),
        )
        .unwrap_err();
        assert_eq!(error.code(), "malformed_payload");
    }

    #[test]
    fn a_buffer_asset_decodes_and_is_bounded_by_the_declared_count() {
        let splat = crate::fixtures::axis_fixture();
        let bytes = encode_buffers(&splat.points, false);
        let checksum = ArtifactChecksum::of(&bytes);
        let decoded = decode_payload(
            AssetKind::SplatBuffers,
            &bytes,
            "batch.buf",
            &checksum,
            &AssetBudgets::default(),
        )
        .unwrap();
        assert_eq!(decoded.len(), splat.len());
        assert_eq!(decoded.points[1].color, splat.points[1].color);

        // The registry's own path checks the declared count before decoding.
        let registry = AssetRegistry::with_session(3, AssetBudgets::default());
        let info = registry
            .register_bytes(AssetKind::SplatBuffers, bytes, "batch.buf")
            .unwrap();
        assert_eq!(info.point_count, Some(splat.len()));
        let handle = registry.resolve(&info.asset_id).unwrap();
        let small = AssetBudgets {
            max_expanded_points: 1,
            ..AssetBudgets::default()
        };
        let error = decode_points(&handle, &small).unwrap_err();
        assert_eq!(error.code(), "budget_exceeded");

        // An attribute-patch asset is not a merge source, and says so.
        let patch = registry
            .register_bytes(AssetKind::AttributePatch, vec![0u8; 12], "values")
            .unwrap();
        let handle = registry.resolve(&patch.asset_id).unwrap();
        assert_eq!(
            decode_points(&handle, &AssetBudgets::default())
                .unwrap_err()
                .code(),
            "unknown_asset_kind"
        );
    }
}
