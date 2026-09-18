//! The desktop's asset host: one registry, shared by the bridge and the window.
//!
//! [`AssetHost`] is the adapter between the wire contract in `splatmcp_bridge` and the
//! registry in `splatmcp_core::asset`. It owns the one registry the process has, so a tool
//! call and a window action address the same assets; nothing here keeps a second copy of a
//! payload, and nothing here decodes a scene to describe it.
//!
//! Two rules come from the registry and are worth repeating at this layer:
//!
//! - a **file** reference must be absolute and is read once, so a later edit of that file
//!   cannot change queued work;
//! - a **patch** is planned (decoded, converted and budget-checked) before any transaction
//!   starts, so a malformed payload is an error on the request rather than a partial commit.

use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::Value;
use splatmcp_bridge::{
    AssetInfoReply, AssetQueryRequest, AssetRegisterReply, AssetRegisterRequest,
    AssetReleaseRequest, AssetStatsSummary, AssetSummary, AssetUploadBeginRequest,
    AssetUploadChunkRequest, AssetUploadReply, AssetUploadRequest, AttributePatchParams, Method,
};
use splatmcp_core::asset::{
    AssetBudgets, AttributePatch, PatchAttribute, PatchDescriptor, PatchDtype, PatchEncoding,
    PatchEndian, PatchLayout, PatchShape,
};
use splatmcp_core::{AssetError, AssetId, AssetKind, AssetRegistry};

/// The process-wide asset registry.
pub struct AssetHost {
    registry: Arc<AssetRegistry>,
}

impl Default for AssetHost {
    fn default() -> Self {
        Self::new(AssetBudgets::default())
    }
}

impl AssetHost {
    /// A host whose registry enforces `budgets`.
    pub fn new(budgets: AssetBudgets) -> Self {
        Self {
            registry: Arc::new(AssetRegistry::with_session(
                splatmcp_core::document::now_ms() ^ (std::process::id() as u64).rotate_left(19),
                budgets,
            )),
        }
    }

    /// The registry itself, for a caller that already has typed work to do.
    pub fn registry(&self) -> &Arc<AssetRegistry> {
        &self.registry
    }

    /// The budgets in force, for a capabilities reply.
    pub fn budgets(&self) -> AssetBudgets {
        self.registry.budgets()
    }

    /// Handles one asset-related bridge method, or `None` when the method is not ours.
    pub fn handle(&self, method: Method, params: Value) -> Option<Result<Value, String>> {
        let result = match method {
            Method::AssetRegister => self.register(params),
            Method::AssetInfo => self.info(params),
            Method::AssetRelease => self.release(params),
            Method::AssetUploadBegin => self.upload_begin(params),
            Method::AssetUploadChunk => self.upload_chunk(params),
            Method::AssetUploadStatus => self.upload_status(params),
            Method::AssetUploadFinalize => self.upload_finalize(params),
            Method::AssetUploadCancel => self.upload_cancel(params),
            _ => return None,
        };
        Some(result)
    }

    /// Registers a file or a small inline payload under a fresh id.
    fn register(&self, params: Value) -> Result<Value, String> {
        let request: AssetRegisterRequest = serde_json::from_value(params)
            .map_err(|error| format!("invalid asset.register request: {error}"))?;
        let kind = kind_of(&request.kind)?;
        let info = match (&request.path, &request.bytes_base64) {
            (Some(path), None) => {
                self.registry
                    .register_file_with(kind, std::path::Path::new(path), request.checksum)
            }
            (None, Some(encoded)) => {
                let bytes = BASE64
                    .decode(encoded.as_bytes())
                    .map_err(|error| format!("bytes_base64 is not valid base64: {error}"))?;
                let label = request
                    .label
                    .clone()
                    .unwrap_or_else(|| format!("inline {} bytes", bytes.len()));
                self.registry
                    .register_with(kind, bytes, label, request.checksum)
            }
            (Some(_), Some(_)) => {
                return Err(
                    "give either path or bytes_base64, not both: an asset has one source"
                        .to_owned(),
                );
            }
            (None, None) => {
                return Err(
                    "give path (a local file) or bytes_base64 (a small inline payload)".to_owned(),
                );
            }
        }
        .map_err(describe_asset_error)?;
        let stats = self.stats()?;
        serde_json::to_value(AssetRegisterReply {
            asset: AssetSummary::from(&info),
            stats,
        })
        .map_err(|error| error.to_string())
    }

    /// One asset's bounded description, or every live asset.
    fn info(&self, params: Value) -> Result<Value, String> {
        let request: AssetQueryRequest = if params.is_null() {
            AssetQueryRequest::default()
        } else {
            serde_json::from_value(params)
                .map_err(|error| format!("invalid asset.info request: {error}"))?
        };
        let reply = match request.asset_id {
            Some(text) => {
                let id =
                    AssetId::parse(&text).ok_or_else(|| format!("'{text}' is not an asset id"))?;
                let info = self.registry.info(&id).map_err(describe_asset_error)?;
                AssetInfoReply {
                    asset: Some(AssetSummary::from(&info)),
                    assets: Vec::new(),
                    stats: self.stats()?,
                }
            }
            None => AssetInfoReply {
                asset: None,
                assets: self
                    .registry
                    .list()
                    .map_err(describe_asset_error)?
                    .iter()
                    .map(AssetSummary::from)
                    .collect(),
                stats: self.stats()?,
            },
        };
        serde_json::to_value(reply).map_err(|error| error.to_string())
    }

    /// Forgets an asset id.
    fn release(&self, params: Value) -> Result<Value, String> {
        let request: AssetReleaseRequest = serde_json::from_value(params)
            .map_err(|error| format!("invalid asset.release request: {error}"))?;
        let id = AssetId::parse(&request.asset_id)
            .ok_or_else(|| format!("'{}' is not an asset id", request.asset_id))?;
        let released = self.registry.release(&id).map_err(describe_asset_error)?;
        Ok(serde_json::json!({
            "released": released,
            "asset_id": request.asset_id
        }))
    }

    /// Stages a chunked upload.
    fn upload_begin(&self, params: Value) -> Result<Value, String> {
        let request: AssetUploadBeginRequest = serde_json::from_value(params)
            .map_err(|error| format!("invalid asset.upload_begin request: {error}"))?;
        let kind = kind_of(&request.kind)?;
        let label = request
            .label
            .clone()
            .unwrap_or_else(|| format!("upload of {} bytes", request.declared_bytes));
        let status = self
            .registry
            .begin_upload(kind, request.declared_bytes, request.checksum, label)
            .map_err(describe_asset_error)?;
        serde_json::to_value(AssetUploadReply::from(&status)).map_err(|error| error.to_string())
    }

    /// Appends one chunk at the next offset.
    fn upload_chunk(&self, params: Value) -> Result<Value, String> {
        let request: AssetUploadChunkRequest = serde_json::from_value(params)
            .map_err(|error| format!("invalid asset.upload_chunk request: {error}"))?;
        let chunk = BASE64
            .decode(request.data_base64.as_bytes())
            .map_err(|error| format!("data_base64 is not valid base64: {error}"))?;
        let status = self
            .registry
            .upload_append(request.upload_id, request.offset, &chunk)
            .map_err(describe_asset_error)?;
        serde_json::to_value(AssetUploadReply::from(&status)).map_err(|error| error.to_string())
    }

    /// Resumable status, so a reconnecting client continues instead of restarting.
    fn upload_status(&self, params: Value) -> Result<Value, String> {
        let request: AssetUploadRequest = serde_json::from_value(params)
            .map_err(|error| format!("invalid asset.upload_status request: {error}"))?;
        let status = self
            .registry
            .upload_status(request.upload_id)
            .map_err(describe_asset_error)?;
        serde_json::to_value(AssetUploadReply::from(&status)).map_err(|error| error.to_string())
    }

    /// Finalizes a staged upload, or refuses it whole.
    fn upload_finalize(&self, params: Value) -> Result<Value, String> {
        let request: AssetUploadRequest = serde_json::from_value(params)
            .map_err(|error| format!("invalid asset.upload_finalize request: {error}"))?;
        let info = self
            .registry
            .upload_finalize(request.upload_id)
            .map_err(describe_asset_error)?;
        let mut reply = AssetUploadReply {
            asset: Some(AssetSummary::from(&info)),
            ..AssetUploadReply::default()
        };
        reply.upload_id = request.upload_id;
        serde_json::to_value(reply).map_err(|error| error.to_string())
    }

    /// Abandons a staged upload.
    fn upload_cancel(&self, params: Value) -> Result<Value, String> {
        let request: AssetUploadRequest = serde_json::from_value(params)
            .map_err(|error| format!("invalid asset.upload_cancel request: {error}"))?;
        let cancelled = self
            .registry
            .upload_cancel(request.upload_id)
            .map_err(describe_asset_error)?;
        Ok(serde_json::json!({
            "upload_id": request.upload_id,
            "cancelled": cancelled
        }))
    }

    /// Live-asset accounting plus the budgets it is measured against.
    fn stats(&self) -> Result<AssetStatsSummary, String> {
        let stats = self.registry.stats().map_err(describe_asset_error)?;
        Ok(AssetStatsSummary::from(&stats).with_budgets(&self.budgets()))
    }

    /// Bytes of a registered asset, resolved by id.
    ///
    /// Returns the handle as well, so the caller holds the snapshot while it works: an
    /// eviction or a release in the meantime cannot change what it is reading.
    pub fn resolve(&self, asset_id: &str) -> Result<splatmcp_core::AssetHandle, String> {
        let id =
            AssetId::parse(asset_id).ok_or_else(|| format!("'{asset_id}' is not an asset id"))?;
        self.registry.resolve(&id).map_err(describe_asset_error)
    }

    /// Decodes the gaussians of a merge source, under the registry's budgets.
    pub fn merge_points(
        &self,
        asset_id: &str,
    ) -> Result<splatmcp_core::asset::GaussianAsset, String> {
        let handle = self.resolve(asset_id)?;
        splatmcp_core::asset::decode_points(&handle, &self.budgets()).map_err(describe_asset_error)
    }

    /// Plans a typed binary attribute patch, or explains what is wrong with it.
    ///
    /// `rows` is the number of gaussians the step targets, when the caller already knows it:
    /// declaring it turns a mismatch into a request error instead of a transaction failure.
    pub fn plan_patch(
        &self,
        params: &AttributePatchParams,
        rows: Option<usize>,
    ) -> Result<AttributePatch, String> {
        let attribute = PatchAttribute::parse(&params.attribute)
            .ok_or_else(|| patch_message(&params.attribute))?;
        let dtype = match &params.dtype {
            Some(text) => PatchDtype::parse(text).ok_or_else(|| {
                format!("unknown dtype '{text}'; use f32, f64, i32, i16, u16 or u8")
            })?,
            None => PatchDtype::F32,
        };
        let shape = match &params.shape {
            Some(shape) => PatchShape::parse(shape).map_err(|error| error.to_string())?,
            None => PatchShape::of(attribute),
        };
        let shape = match rows {
            Some(rows) => PatchShape {
                rows: Some(rows),
                ..shape
            },
            None => shape,
        };
        let layout = match &params.layout {
            Some(text) => PatchLayout::parse(text).ok_or_else(|| {
                format!(
                    "unknown layout '{text}'; only 'scalar' (tightly packed scalars) is defined"
                )
            })?,
            None => PatchLayout::Scalar,
        };
        let endian = match &params.endian {
            Some(text) => PatchEndian::parse(text)
                .ok_or_else(|| format!("unknown endian '{text}'; use little or big"))?,
            None => PatchEndian::Little,
        };
        let encoding = match &params.encoding {
            Some(text) => PatchEncoding::parse(text)
                .ok_or_else(|| format!("unknown encoding '{text}'; use activated or serialized"))?,
            None => PatchEncoding::Activated,
        };
        let descriptor = PatchDescriptor {
            attribute,
            dtype,
            shape,
            layout,
            endian,
            encoding,
        };

        let (bytes, label) = match (&params.asset_id, &params.values_base64) {
            (Some(asset_id), None) => {
                let handle = self.resolve(asset_id)?;
                (handle.bytes().to_vec(), format!("asset {asset_id}"))
            }
            (None, Some(encoded)) => {
                let bytes = BASE64
                    .decode(encoded.as_bytes())
                    .map_err(|error| format!("values_base64 is not valid base64: {error}"))?;
                (bytes, "inline values".to_owned())
            }
            (Some(_), Some(_)) => {
                return Err(
                    "give either patch.asset_id or patch.values_base64, not both".to_owned(),
                );
            }
            (None, None) => {
                return Err(
                    "a patch needs patch.asset_id (registered values) or patch.values_base64"
                        .to_owned(),
                );
            }
        };
        AttributePatch::plan(&bytes, label, descriptor, &self.budgets())
            .map_err(|error| format!("{} ({})", error, error.code()))
    }
}

/// Parses an asset kind name, or explains which names exist.
pub fn kind_of(text: &str) -> Result<AssetKind, String> {
    AssetKind::parse(text).ok_or_else(|| {
        format!("unknown asset kind '{text}'; use ply, splat_buffers or attribute_patch")
    })
}

/// A patch attribute name that is not one of the contract's.
fn patch_message(attribute: &str) -> String {
    format!("unknown attribute '{attribute}'; use position, scale, rotation, color or opacity")
}

/// An asset failure as a sentence a caller can act on, with its stable code.
fn describe_asset_error(error: AssetError) -> String {
    format!("{} ({})", error, error.code())
}

/// Tauri command: register an asset from a file path or a small inline payload.
#[tauri::command]
pub fn asset_register(
    host: tauri::State<'_, AssetHostState>,
    request: Value,
) -> Result<Value, String> {
    host.0.handle(Method::AssetRegister, request).unwrap()
}

/// Tauri command: one asset's description, or every live asset.
#[tauri::command]
pub fn asset_info(
    host: tauri::State<'_, AssetHostState>,
    request: Option<Value>,
) -> Result<Value, String> {
    host.0
        .handle(Method::AssetInfo, request.unwrap_or(Value::Null))
        .unwrap()
}

/// Tauri command: forget an asset id.
#[tauri::command]
pub fn asset_release(
    host: tauri::State<'_, AssetHostState>,
    request: Value,
) -> Result<Value, String> {
    host.0.handle(Method::AssetRelease, request).unwrap()
}

/// Tauri command: begin, append to, finalize or cancel a chunked upload.
///
/// One command with an `action` field, so the window and the MCP client drive the same four
/// operations through one shared implementation.
#[tauri::command]
pub fn asset_upload(
    host: tauri::State<'_, AssetHostState>,
    action: String,
    request: Value,
) -> Result<Value, String> {
    let method = match action.as_str() {
        "begin" => Method::AssetUploadBegin,
        "append" => Method::AssetUploadChunk,
        "status" => Method::AssetUploadStatus,
        "finalize" => Method::AssetUploadFinalize,
        "cancel" => Method::AssetUploadCancel,
        other => {
            return Err(format!(
                "unknown upload action '{other}'; use begin, append, status, finalize or cancel"
            ));
        }
    };
    host.0.handle(method, request).unwrap()
}

/// Managed state wrapper so the Tauri commands and the bridge share one registry.
pub struct AssetHostState(pub Arc<AssetHost>);

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> AssetHost {
        AssetHost::new(AssetBudgets::default())
    }

    #[test]
    fn a_registered_file_is_described_without_touching_its_bytes() {
        let directory = std::env::temp_dir().join(format!("splatmcp-host-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("scene.ply");
        let splat = splatmcp_core::fixtures::axis_fixture();
        std::fs::write(&path, splatmcp_core::write_ply(&splat).unwrap()).unwrap();

        let host = host();
        let reply = host
            .register(serde_json::json!({
                "path": path.to_string_lossy(),
                "kind": "ply"
            }))
            .unwrap();
        let reply: AssetRegisterReply = serde_json::from_value(reply).unwrap();
        assert_eq!(reply.asset.kind, "ply");
        assert_eq!(reply.asset.point_count, Some(splat.len()));
        assert!(reply.stats.budgets.contains("asset_bytes<="));

        // The same bytes are reachable through the merge path, and the description of the
        // reply is bounded: no geometry travels in it.
        let merged = host.merge_points(&reply.asset.asset_id).unwrap();
        assert_eq!(merged.len(), splat.len());
        assert!(reply.asset.provenance.ends_with("scene.ply"));

        let released = host
            .release(serde_json::json!({ "asset_id": reply.asset.asset_id }))
            .unwrap();
        assert_eq!(released["released"], true);
        let error = host.merge_points(&reply.asset.asset_id).unwrap_err();
        assert!(error.contains("not known to this app"), "{error}");
        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn a_patch_is_planned_from_an_asset_or_from_inline_values() {
        let host = host();
        let values: Vec<u8> = [0.25f32, 0.5, 0.75]
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect();
        let registered = host
            .register(serde_json::json!({
                "bytes_base64": BASE64.encode(&values),
                "kind": "attribute_patch",
                "label": "colors"
            }))
            .unwrap();
        let asset_id = registered["asset"]["asset_id"].as_str().unwrap().to_owned();

        let params = AttributePatchParams {
            attribute: "color".to_owned(),
            asset_id: Some(asset_id),
            ..AttributePatchParams::default()
        };
        let patch = host.plan_patch(&params, Some(1)).unwrap();
        assert_eq!(patch.rows(), 1);
        assert_eq!(patch.descriptor().dtype, PatchDtype::F32);
        assert_eq!(patch.descriptor().encoding, PatchEncoding::Activated);
        assert_eq!(patch.source_bytes(), 12);

        // The inline form decodes identically, and the two are the same payload.
        let inline = host
            .plan_patch(
                &AttributePatchParams {
                    attribute: "colour".to_owned(),
                    values_base64: Some(BASE64.encode(&values)),
                    ..AttributePatchParams::default()
                },
                Some(1),
            )
            .unwrap();
        assert_eq!(inline.payload_hash(), patch.payload_hash());

        // Declaring the wrong row count is a request error, not a transaction failure.
        let error = host.plan_patch(&params, Some(3)).unwrap_err();
        assert!(error.contains("payload_length_mismatch"), "{error}");

        // An unknown attribute, layout or endianness says what is accepted.
        for (params, fragment) in [
            (
                AttributePatchParams {
                    attribute: "hardness".to_owned(),
                    ..params.clone()
                },
                "unknown attribute",
            ),
            (
                AttributePatchParams {
                    layout: Some("raw-memory".to_owned()),
                    ..params.clone()
                },
                "only 'scalar'",
            ),
            (
                AttributePatchParams {
                    endian: Some("middle".to_owned()),
                    ..params.clone()
                },
                "unknown endian",
            ),
        ] {
            let error = host.plan_patch(&params, Some(1)).unwrap_err();
            assert!(error.contains(fragment), "{error}");
        }

        // A serialized position has no defined conversion.
        let error = host
            .plan_patch(
                &AttributePatchParams {
                    attribute: "position".to_owned(),
                    encoding: Some("serialized".to_owned()),
                    values_base64: Some(BASE64.encode(&values)),
                    ..AttributePatchParams::default()
                },
                Some(1),
            )
            .unwrap_err();
        assert!(error.contains("no 'serialized' conversion"), "{error}");
    }

    #[test]
    fn a_chunked_upload_is_resumable_and_finalized_whole() {
        let host = host();
        let splat = splatmcp_core::fixtures::axis_fixture();
        let bytes = splatmcp_core::write_ply(&splat).unwrap();
        let checksum = splatmcp_core::ArtifactChecksum::of(&bytes);

        let begun = host
            .upload_begin(serde_json::json!({
                "kind": "ply",
                "declared_bytes": bytes.len(),
                "checksum": checksum.value,
                "label": "streamed scene"
            }))
            .unwrap();
        let upload_id = begun["upload_id"].as_u64().unwrap();
        assert_eq!(begun["complete"], false);

        let half = bytes.len() / 2;
        let status = host
            .upload_chunk(serde_json::json!({
                "upload_id": upload_id,
                "offset": 0,
                "data_base64": BASE64.encode(&bytes[..half])
            }))
            .unwrap();
        assert_eq!(status["next_offset"].as_u64().unwrap() as usize, half);

        // A reconnecting client can ask where it stopped instead of guessing.
        let resumed = host
            .upload_status(serde_json::json!({ "upload_id": upload_id }))
            .unwrap();
        assert_eq!(resumed["next_offset"].as_u64().unwrap() as usize, half);

        // An out-of-order chunk is refused with the offset to use.
        let error = host
            .upload_chunk(serde_json::json!({
                "upload_id": upload_id,
                "offset": half - 1,
                "data_base64": BASE64.encode(&bytes[..4])
            }))
            .unwrap_err();
        assert!(error.contains("upload_offset_mismatch"), "{error}");

        host.upload_chunk(serde_json::json!({
            "upload_id": upload_id,
            "offset": half,
            "data_base64": BASE64.encode(&bytes[half..])
        }))
        .unwrap();
        let finalized = host
            .upload_finalize(serde_json::json!({ "upload_id": upload_id }))
            .unwrap();
        let asset_id = finalized["asset"]["asset_id"].as_str().unwrap().to_owned();
        assert_eq!(finalized["asset"]["point_count"], splat.len());
        assert!(host.merge_points(&asset_id).is_ok());

        // A cancelled upload is simply gone, and its id cannot be appended to.
        let other = host
            .upload_begin(serde_json::json!({ "kind": "ply", "declared_bytes": 4 }))
            .unwrap();
        let other_id = other["upload_id"].as_u64().unwrap();
        assert_eq!(
            host.upload_cancel(serde_json::json!({ "upload_id": other_id }))
                .unwrap()["cancelled"],
            true
        );
        assert!(
            host.upload_status(serde_json::json!({ "upload_id": other_id }))
                .unwrap_err()
                .contains("not staged")
        );
    }

    #[test]
    fn the_info_reply_lists_live_assets_with_the_budgets_in_force() {
        let host = host();
        host.register(serde_json::json!({
            "bytes_base64": BASE64.encode(
                b"ply\nformat ascii 1.0\nelement vertex 1\nproperty float x\nproperty float y\n\
                  property float z\nend_header\n0 0 0\n"
            ),
            "kind": "ply"
        }))
        .unwrap();
        let reply: AssetInfoReply =
            serde_json::from_value(host.info(Value::Null).unwrap()).unwrap();
        assert_eq!(reply.assets.len(), 1);
        assert_eq!(reply.stats.assets, 1);
        assert!(reply.stats.budgets.contains("expanded_points<="));
        assert!(reply.asset.is_none());
    }
}
