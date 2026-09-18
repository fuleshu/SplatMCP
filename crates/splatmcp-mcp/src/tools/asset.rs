//! Asset tools: compact payload references instead of inline geometry.
//!
//! A tool that would otherwise carry a 500 000 gaussian PLY - or the values for one
//! attribute of it - registers the payload once and then names it. The desktop app owns the
//! bytes from then on, so:
//!
//! - a **file** is snapshotted under the app's own file authorization (absolute path only),
//!   and editing it afterwards cannot change work that is already queued;
//! - the id is the identity, so nothing but bounded metadata crosses the bridge;
//! - the budgets the app enforces are reported back, so a caller sees the real ceiling
//!   instead of guessing at one.
//!
//! The chunked-upload methods on the bridge serve a client that cannot reach the file; this
//! tool registers a local file or a small inline payload, which is the same-host case.

use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use splatmcp_bridge::{AssetInfoReply, AssetRegisterReply, Method};

use crate::bridge::AppLink;

/// Registers a payload the app will hold as an immutable asset.
#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
pub struct RegisterAssetInput {
    /// Absolute path of the file to snapshot; preferred for large payloads.
    #[serde(default)]
    pub path: Option<String>,
    /// Base64 of a small inline payload, when there is no file.
    #[serde(default)]
    pub bytes_base64: Option<String>,
    /// ply | splat_buffers | attribute_patch.
    pub kind: String,
    /// FNV-1a 64 of the bytes, when known: a mismatch is refused.
    #[serde(default)]
    pub checksum: Option<u64>,
    /// Label recorded as provenance.
    #[serde(default)]
    pub label: Option<String>,
}

/// Describes one asset, lists live assets, or releases one.
#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
pub struct AssetInfoInput {
    /// The asset to describe; omit to list every live asset.
    #[serde(default)]
    pub asset_id: Option<String>,
    /// Forget the asset id when true.
    #[serde(default)]
    pub release: Option<bool>,
}

/// Bounded description of the asset a registration produced.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RegisterReply {
    pub asset_id: String,
    pub kind: String,
    pub schema: String,
    pub bytes: usize,
    pub checksum_value: u64,
    pub provenance: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub point_count: Option<usize>,
    /// The limits the app enforces, verbatim.
    pub budgets: String,
}

impl RegisterReply {
    fn of(reply: &AssetRegisterReply) -> Self {
        Self {
            asset_id: reply.asset.asset_id.clone(),
            kind: reply.asset.kind.clone(),
            schema: reply.asset.schema.clone(),
            bytes: reply.asset.bytes,
            checksum_value: reply.asset.checksum_value,
            provenance: reply.asset.provenance.clone(),
            point_count: reply.asset.point_count,
            budgets: reply.stats.budgets.clone(),
        }
    }
}

/// Registers a file or a small inline payload and returns its id and bounded metadata.
pub fn register(link: &AppLink, input: &RegisterAssetInput) -> Result<RegisterReply, String> {
    let request = serde_json::json!({
        "path": input.path,
        "bytes_base64": input.bytes_base64,
        "kind": input.kind,
        "checksum": input.checksum,
        "label": input.label,
    });
    let reply: AssetRegisterReply = link
        .request_typed(Method::AssetRegister, request)
        .map_err(|error| format!("{error}"))?;
    // A registered id is only useful with its own numbers: it is never a path.
    Ok(RegisterReply::of(&reply))
}

/// Describes, lists or releases assets.
pub fn info(link: &AppLink, input: &AssetInfoInput) -> Result<Value, String> {
    if input.release.unwrap_or(false) {
        let Some(asset_id) = input.asset_id.clone() else {
            return Err("releasing needs asset_id; omit release to list live assets".to_owned());
        };
        return link
            .request(
                Method::AssetRelease,
                serde_json::json!({ "asset_id": asset_id }),
            )
            .map_err(|error| format!("{error}"));
    }
    match &input.asset_id {
        Some(asset_id) => {
            let reply: AssetInfoReply = link
                .request_typed(
                    Method::AssetInfo,
                    serde_json::json!({ "asset_id": asset_id }),
                )
                .map_err(|error| format!("{error}"))?;
            let asset = reply
                .asset
                .ok_or_else(|| format!("the app did not describe asset {asset_id}"))?;
            let encoded = serde_json::json!({
                "asset": Summary::of(&asset),
                "budgets": reply.stats.budgets,
            });
            Ok(encoded)
        }
        None => {
            let reply: AssetInfoReply = link
                .request_typed(Method::AssetInfo, Value::Null)
                .map_err(|error| format!("{error}"))?;
            let assets: Vec<Summary> = reply.assets.iter().map(Summary::of).collect();
            let encoded = serde_json::json!({
                "assets": assets,
                "count": assets.len(),
                "stats": {
                    "bytes": reply.stats.bytes,
                    "uploads": reply.stats.uploads,
                    "evicted": reply.stats.evicted,
                    "budgets": reply.stats.budgets,
                }
            });
            Ok(encoded)
        }
    }
}

/// Bounded summary of one asset, as a tool reply reports it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Summary {
    pub asset_id: String,
    pub kind: String,
    pub schema: String,
    pub media_type: String,
    pub bytes: usize,
    pub checksum_value: u64,
    pub provenance: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub point_count: Option<usize>,
    pub expires_at_ms: u64,
}

impl Summary {
    fn of(asset: &splatmcp_bridge::AssetSummary) -> Self {
        Self {
            asset_id: asset.asset_id.clone(),
            kind: asset.kind.clone(),
            schema: asset.schema.clone(),
            media_type: asset.media_type.clone(),
            bytes: asset.bytes,
            checksum_value: asset.checksum_value,
            provenance: asset.provenance.clone(),
            point_count: asset.point_count,
            expires_at_ms: asset.expires_at_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_registration_is_asked_for_with_the_source_it_was_given() {
        let input = RegisterAssetInput {
            path: Some("C:/scenes/house.ply".to_owned()),
            bytes_base64: None,
            kind: "ply".to_owned(),
            checksum: Some(0x1234),
            label: None,
        };
        let request = serde_json::json!({
            "path": input.path,
            "bytes_base64": input.bytes_base64,
            "kind": input.kind,
            "checksum": input.checksum,
            "label": input.label,
        });
        assert_eq!(request["kind"], "ply");
        assert_eq!(request["path"], "C:/scenes/house.ply");
        assert!(request["bytes_base64"].is_null());
        // The reply is bounded: an id and numbers, never the payload.
        let reply = RegisterReply::of(&AssetRegisterReply {
            asset: splatmcp_bridge::AssetSummary {
                asset_id: "asset-4f2a-1".to_owned(),
                kind: "ply".to_owned(),
                contract_version: 1,
                media_type: "application/x-ply".to_owned(),
                schema: "ply".to_owned(),
                bytes: 12_000_000,
                checksum_value: 7,
                provenance: "C:/scenes/house.ply".to_owned(),
                point_count: Some(500_000),
                created_at_ms: 1,
                expires_at_ms: 2,
            },
            stats: splatmcp_bridge::AssetStatsSummary {
                budgets: "asset_bytes<=536870912".to_owned(),
                ..splatmcp_bridge::AssetStatsSummary::default()
            },
        });
        let encoded = serde_json::to_string(&reply).unwrap();
        assert!(encoded.contains("asset-4f2a-1"));
        assert!(encoded.contains("500000"));
        assert!(encoded.len() < 300, "{encoded}");
        assert!(!encoded.contains("base64"));
    }

    #[test]
    fn releasing_without_an_id_is_refused_before_any_call() {
        let link = AppLink::new(false);
        let error = info(
            &link,
            &AssetInfoInput {
                asset_id: None,
                release: Some(true),
            },
        )
        .unwrap_err();
        assert!(error.contains("needs asset_id"), "{error}");
    }
}
