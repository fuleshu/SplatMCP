//! The desktop-owned registry of immutable assets.
//!
//! Everything a caller submits - a file path, or a chunked upload - becomes one entry here.
//! An entry is addressed by [`AssetId`] only; its bytes are shared behind an [`AssetHandle`],
//! so a decode can hold a consistent snapshot while the registry moves on.
//!
//! Two behaviours are deliberate and worth stating:
//!
//! - **Snapshot once.** A file is read to completion at registration and never read again,
//!   so editing it afterwards cannot change work that is already queued.
//! - **Eviction never invalidates a live holder.** Expiry and the byte/count ceilings remove
//!   the *id*; an [`AssetHandle`] already resolved by a caller keeps its bytes alive, and a
//!   later lookup of that id fails with [`AssetError::UnknownAsset`] (or
//!   [`AssetError::Expired`] while the entry is still known but past its lifetime).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::asset::ply_probe;
use crate::asset::{
    ASSET_CONTRACT_VERSION, AssetBudgets, AssetError, AssetId, AssetInfo, AssetKind, AssetStats,
    buffers, checksum_of,
};
use crate::document::{ArtifactChecksum, now_ms};

/// Bytes of one asset, shared by every holder of it.
#[derive(Debug)]
pub struct Asset {
    info: AssetInfo,
    bytes: Arc<[u8]>,
}

impl Asset {
    /// Bounded description of these bytes.
    pub fn info(&self) -> &AssetInfo {
        &self.info
    }

    pub fn id(&self) -> &AssetId {
        &self.info.asset_id
    }

    pub fn kind(&self) -> AssetKind {
        self.info.kind
    }

    /// The bytes themselves, valid for as long as this handle is held.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn checksum(&self) -> ArtifactChecksum {
        self.info.checksum
    }

    /// Gaussians the payload declares, when its header could be scanned at registration.
    pub fn point_count(&self) -> Option<usize> {
        self.info.point_count
    }

    /// Where the bytes came from.
    pub fn source(&self) -> &str {
        &self.info.provenance
    }
}

/// A resolved asset. Holding it keeps the bytes alive, whatever the registry does next.
pub type AssetHandle = Arc<Asset>;

/// Bytes and lifetime of one entry in the registry.
struct Entry {
    info: AssetInfo,
    bytes: Arc<[u8]>,
    last_used_ms: u64,
}

/// One staged upload that is still receiving chunks.
struct StagedUpload {
    upload_id: u64,
    kind: AssetKind,
    declared_bytes: u64,
    declared_checksum: Option<u64>,
    provenance: String,
    buffer: Vec<u8>,
    last_ms: u64,
}

impl StagedUpload {
    fn expires_at(&self, lifetime_ms: u64) -> Option<u64> {
        (lifetime_ms > 0).then(|| self.last_ms + lifetime_ms)
    }
}

/// Registry contents, kept behind one lock so an asset never half-an-exists.
#[derive(Default)]
struct Inner {
    entries: BTreeMap<u64, Entry>,
    uploads: BTreeMap<u64, StagedUpload>,
    next_index: u64,
    next_upload: u64,
    evicted: u64,
}

impl Inner {
    fn bytes(&self) -> u64 {
        self.entries
            .values()
            .map(|entry| entry.bytes.len() as u64)
            .sum()
    }

    fn upload_bytes(&self) -> u64 {
        self.uploads
            .values()
            .map(|upload| upload.buffer.len() as u64)
            .sum()
    }

    /// Drops entries and uploads past their lifetime, returning how many went.
    fn sweep(&mut self, now: u64, lifetime_ms: u64) -> usize {
        let before = self.entries.len() + self.uploads.len();
        let expired: Vec<u64> = self
            .entries
            .iter()
            .filter(|(_, entry)| !entry.info.is_live(now))
            .map(|(index, _)| *index)
            .collect();
        for index in expired {
            self.entries.remove(&index);
        }
        let stale: Vec<u64> = self
            .uploads
            .iter()
            .filter(|(_, upload)| upload.expires_at(lifetime_ms).is_some_and(|at| at <= now))
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            self.uploads.remove(&id);
        }
        let removed = before - (self.entries.len() + self.uploads.len());
        self.evicted += removed as u64;
        removed
    }

    /// Drops the least recently used entry, or reports that nothing can go.
    fn evict_lru(&mut self) -> bool {
        let oldest = self
            .entries
            .iter()
            .min_by_key(|(index, entry)| (entry.last_used_ms, **index))
            .map(|(index, _)| *index);
        match oldest {
            Some(index) => {
                self.entries.remove(&index);
                self.evicted += 1;
                true
            }
            None => false,
        }
    }
}

/// The registry itself: minting ids, enforcing budgets and owning the snapshots.
pub struct AssetRegistry {
    session: u64,
    budgets: AssetBudgets,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    inner: Mutex<Inner>,
}

impl Default for AssetRegistry {
    fn default() -> Self {
        // A session stamp that differs between runs, so an id from an earlier run cannot
        // name one of today's assets.
        let session = now_ms() ^ (std::process::id() as u64).rotate_left(17);
        Self::with_session(session, AssetBudgets::default())
    }
}

impl AssetRegistry {
    /// A registry whose ids are stamped with `session` and whose limits are `budgets`.
    pub fn with_session(session: u64, budgets: AssetBudgets) -> Self {
        Self {
            session,
            budgets,
            clock: Arc::new(now_ms),
            inner: Mutex::new(Inner::default()),
        }
    }

    /// The same registry, reading time from `clock`. Used by tests that need a fixed clock.
    pub fn with_clock(
        session: u64,
        budgets: AssetBudgets,
        clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    ) -> Self {
        Self {
            session,
            budgets,
            clock,
            inner: Mutex::new(Inner::default()),
        }
    }

    /// The limits this registry enforces, for a capabilities reply.
    pub fn budgets(&self) -> AssetBudgets {
        self.budgets
    }

    fn now(&self) -> u64 {
        (self.clock)()
    }

    fn locked(&self) -> Result<MutexGuard<'_, Inner>, AssetError> {
        self.inner.lock().map_err(|error| AssetError::Unavailable {
            reason: error.to_string(),
        })
    }

    /// Registers bytes a caller already holds, snapshotting them under a fresh id.
    pub fn register_bytes(
        &self,
        kind: AssetKind,
        bytes: Vec<u8>,
        provenance: impl Into<String>,
    ) -> Result<AssetInfo, AssetError> {
        self.register_with(kind, bytes, provenance.into(), None)
    }

    /// Registers a local file: read once, validated, and never read again.
    ///
    /// The path must be absolute, so nothing resolves against the process working directory.
    pub fn register_file(&self, kind: AssetKind, path: &Path) -> Result<AssetInfo, AssetError> {
        self.register_file_with(kind, path, None)
    }

    /// The same, checking the bytes against a checksum the caller declared.
    pub fn register_file_with(
        &self,
        kind: AssetKind,
        path: &Path,
        declared_checksum: Option<u64>,
    ) -> Result<AssetInfo, AssetError> {
        if !path.is_absolute() {
            return Err(AssetError::RelativePath {
                path: path.to_string_lossy().to_string(),
            });
        }
        let display = path.to_string_lossy().to_string();
        let metadata = std::fs::metadata(path).map_err(|error| AssetError::Io {
            path: display.clone(),
            reason: error.to_string(),
        })?;
        if !metadata.is_file() {
            return Err(AssetError::Io {
                path: display,
                reason: "not a regular file".to_owned(),
            });
        }
        self.budgets.check_declared(metadata.len(), "the file")?;
        let bytes = std::fs::read(path).map_err(|error| AssetError::Io {
            path: display.clone(),
            reason: error.to_string(),
        })?;
        self.register_with(kind, bytes, display, declared_checksum)
    }

    /// Registers bytes, checking the declared size, checksum and payload shape first.
    pub fn register_with(
        &self,
        kind: AssetKind,
        bytes: Vec<u8>,
        provenance: String,
        declared_checksum: Option<u64>,
    ) -> Result<AssetInfo, AssetError> {
        self.budgets.check_declared(bytes.len() as u64, "the asset")?;
        let checksum = checksum_of(&bytes);
        if let Some(expected) = declared_checksum
            && expected != checksum.value
        {
            return Err(AssetError::HashMismatch {
                expected,
                actual: checksum.value,
            });
        }
        // The payload's own header is checked here, so a malformed or truncated asset is
        // refused at submission instead of at commit.
        let point_count = probe_payload(kind, &bytes)?;
        let now = self.now();
        let bytes: Arc<[u8]> = Arc::from(bytes);

        let mut inner = self.locked()?;
        inner.sweep(now, self.budgets.lifetime_ms);
        while inner.entries.len() >= self.budgets.max_assets
            || inner.bytes() + bytes.len() as u64 > self.budgets.max_total_bytes
        {
            if !inner.evict_lru() {
                return Err(AssetError::RegistryFull {
                    limit: self.budgets.max_assets,
                });
            }
        }
        inner.next_index += 1;
        let index = inner.next_index;
        let asset_id = AssetId::mint(self.session, index);
        let info = AssetInfo {
            asset_id,
            kind,
            contract_version: ASSET_CONTRACT_VERSION,
            media_type: kind.media_type(),
            schema: kind.schema(),
            bytes: bytes.len(),
            checksum,
            provenance,
            point_count,
            created_at_ms: now,
            expires_at_ms: (self.budgets.lifetime_ms > 0)
                .then(|| now + self.budgets.lifetime_ms),
        };
        inner.entries.insert(
            index,
            Entry {
                info: info.clone(),
                bytes,
                last_used_ms: now,
            },
        );
        Ok(info)
    }

    /// Resolves an asset by id, refreshing its place in the eviction order.
    pub fn resolve(&self, asset_id: &AssetId) -> Result<AssetHandle, AssetError> {
        let now = self.now();
        let mut inner = self.locked()?;
        let index = match index_of(&inner, asset_id) {
            Some(index) => index,
            None => {
                return Err(AssetError::UnknownAsset {
                    asset_id: asset_id.clone(),
                });
            }
        };
        let expired = inner
            .entries
            .get(&index)
            .is_some_and(|entry| !entry.info.is_live(now));
        if expired {
            inner.entries.remove(&index);
            inner.evicted += 1;
            return Err(AssetError::Expired {
                asset_id: asset_id.clone(),
            });
        }
        let entry = inner
            .entries
            .get_mut(&index)
            .expect("the entry was found a moment ago");
        entry.last_used_ms = now;
        Ok(Arc::new(Asset {
            info: entry.info.clone(),
            bytes: entry.bytes.clone(),
        }))
    }

    /// Bounded description of an asset without resolving its bytes.
    pub fn info(&self, asset_id: &AssetId) -> Result<AssetInfo, AssetError> {
        Ok(self.resolve(asset_id)?.info().clone())
    }

    /// Every live asset, newest first, with expired entries swept first.
    pub fn list(&self) -> Result<Vec<AssetInfo>, AssetError> {
        let now = self.now();
        let mut inner = self.locked()?;
        inner.sweep(now, self.budgets.lifetime_ms);
        Ok(inner
            .entries
            .values()
            .rev()
            .map(|entry| entry.info.clone())
            .collect())
    }

    /// Forgets an asset id. A caller that already holds a handle keeps its bytes.
    pub fn release(&self, asset_id: &AssetId) -> Result<bool, AssetError> {
        let mut inner = self.locked()?;
        let Some(index) = index_of(&inner, asset_id) else {
            return Ok(false);
        };
        inner.entries.remove(&index);
        inner.evicted += 1;
        Ok(true)
    }

    /// Drops everything past its lifetime, returning how many entries went.
    pub fn sweep(&self) -> Result<usize, AssetError> {
        let now = self.now();
        let mut inner = self.locked()?;
        Ok(inner.sweep(now, self.budgets.lifetime_ms))
    }

    /// Live-asset accounting.
    pub fn stats(&self) -> Result<AssetStats, AssetError> {
        let inner = self.locked()?;
        Ok(AssetStats {
            assets: inner.entries.len(),
            bytes: inner.bytes(),
            uploads: inner.uploads.len(),
            upload_bytes: inner.upload_bytes(),
            evicted: inner.evicted,
        })
    }

    /// Stages a chunked upload and returns its resumable status.
    pub fn begin_upload(
        &self,
        kind: AssetKind,
        declared_bytes: u64,
        declared_checksum: Option<u64>,
        provenance: impl Into<String>,
    ) -> Result<UploadStatus, AssetError> {
        self.budgets.check_declared(declared_bytes, "the upload")?;
        let now = self.now();
        let mut inner = self.locked()?;
        inner.sweep(now, self.budgets.lifetime_ms);
        if inner.bytes() + inner.upload_bytes() + declared_bytes > self.budgets.max_total_bytes {
            return Err(AssetError::TooLarge {
                what: "the staging buffer",
                requested: inner.bytes() + inner.upload_bytes() + declared_bytes,
                limit: self.budgets.max_total_bytes,
            });
        }
        inner.next_upload += 1;
        let upload_id = inner.next_upload;
        let provenance = provenance.into();
        inner.uploads.insert(
            upload_id,
            StagedUpload {
                upload_id,
                kind,
                declared_bytes,
                declared_checksum,
                provenance,
                // Reserved in small steps: a declared length is a promise, not an allocation.
                buffer: Vec::with_capacity((declared_bytes.min(1024 * 1024)) as usize),
                last_ms: now,
            },
        );
        let upload = inner.uploads.get(&upload_id).expect("just staged");
        Ok(status_of(upload, self.budgets.lifetime_ms))
    }

    /// Appends one chunk at the next offset.
    ///
    /// Offsets are sequential by design: a resumed upload continues where it stopped, and a
    /// chunk that would leave a hole is refused with the offset to use instead.
    pub fn upload_append(
        &self,
        upload_id: u64,
        offset: u64,
        chunk: &[u8],
    ) -> Result<UploadStatus, AssetError> {
        if chunk.len() as u64 > self.budgets.max_upload_chunk_bytes {
            return Err(AssetError::UploadChunkTooLarge {
                requested: chunk.len() as u64,
                limit: self.budgets.max_upload_chunk_bytes,
            });
        }
        let now = self.now();
        let mut inner = self.locked()?;
        let lifetime = self.budgets.lifetime_ms;
        inner.sweep(now, lifetime);
        let upload = inner
            .uploads
            .get_mut(&upload_id)
            .ok_or(AssetError::UploadUnknown { upload_id })?;
        let received = upload.buffer.len() as u64;
        if offset != received {
            return Err(AssetError::UploadOffset {
                expected: received,
                given: offset,
            });
        }
        if received + chunk.len() as u64 > upload.declared_bytes {
            return Err(AssetError::TooLarge {
                what: "the upload",
                requested: received + chunk.len() as u64,
                limit: upload.declared_bytes,
            });
        }
        upload.buffer.extend_from_slice(chunk);
        upload.last_ms = now;
        Ok(status_of(upload, lifetime))
    }

    /// Resumable status of a staged upload.
    pub fn upload_status(&self, upload_id: u64) -> Result<UploadStatus, AssetError> {
        let now = self.now();
        let lifetime = self.budgets.lifetime_ms;
        let mut inner = self.locked()?;
        inner.sweep(now, lifetime);
        let upload = inner
            .uploads
            .get(&upload_id)
            .ok_or(AssetError::UploadUnknown { upload_id })?;
        Ok(status_of(upload, lifetime))
    }

    /// Turns a staged upload into a registered asset, or refuses it whole.
    ///
    /// Nothing is registered unless the length and the declared checksum both match, so a
    /// half-uploaded asset cannot be used by accident.
    pub fn upload_finalize(&self, upload_id: u64) -> Result<AssetInfo, AssetError> {
        let now = self.now();
        let mut inner = self.locked()?;
        inner.sweep(now, self.budgets.lifetime_ms);
        let upload = inner
            .uploads
            .get_mut(&upload_id)
            .ok_or(AssetError::UploadUnknown { upload_id })?;
        let received = upload.buffer.len() as u64;
        if received != upload.declared_bytes {
            return Err(AssetError::Truncated {
                declared: upload.declared_bytes,
                received,
            });
        }
        let kind = upload.kind;
        let declared_checksum = upload.declared_checksum;
        let provenance = upload.provenance.clone();
        let bytes = std::mem::take(&mut upload.buffer);
        inner.uploads.remove(&upload_id);
        drop(inner);
        self.register_with(kind, bytes, provenance, declared_checksum)
    }

    /// Abandons a staged upload and frees its staging buffer.
    pub fn upload_cancel(&self, upload_id: u64) -> Result<bool, AssetError> {
        let mut inner = self.locked()?;
        Ok(inner.uploads.remove(&upload_id).is_some())
    }
}

/// Index of the entry an id names, when it is still known.
fn index_of(inner: &Inner, asset_id: &AssetId) -> Option<u64> {
    inner
        .entries
        .iter()
        .find(|(_, entry)| &entry.info.asset_id == asset_id)
        .map(|(index, _)| *index)
}

/// Resumable status of a staged upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadStatus {
    pub upload_id: u64,
    pub kind: AssetKind,
    pub declared_bytes: u64,
    pub received_bytes: u64,
    /// Offset the next chunk must use.
    pub next_offset: u64,
    pub provenance: String,
    /// When an abandoned upload is dropped.
    pub expires_at_ms: u64,
    /// True once every declared byte has arrived (finalize still validates the checksum).
    pub complete: bool,
}

/// One line of progress for a client that keeps a large submission going.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UploadProgress {
    pub upload_id: u64,
    pub received_bytes: u64,
    pub declared_bytes: u64,
}

impl From<&UploadStatus> for UploadProgress {
    fn from(status: &UploadStatus) -> Self {
        Self {
            upload_id: status.upload_id,
            received_bytes: status.received_bytes,
            declared_bytes: status.declared_bytes,
        }
    }
}

/// A staged upload, handed to a caller that wants to stream into it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetUpload {
    pub upload_id: u64,
    pub kind: AssetKind,
    pub declared_bytes: u64,
    pub declared_checksum: Option<u64>,
}

fn status_of(upload: &StagedUpload, lifetime_ms: u64) -> UploadStatus {
    let received = upload.buffer.len() as u64;
    UploadStatus {
        upload_id: upload.upload_id,
        kind: upload.kind,
        declared_bytes: upload.declared_bytes,
        received_bytes: received,
        next_offset: received,
        provenance: upload.provenance.clone(),
        // Zero means "no expiry", so an abandoned staging buffer is never dropped silently
        // when the caller asked for no lifetime.
        expires_at_ms: if lifetime_ms == 0 {
            0
        } else {
            upload.last_ms + lifetime_ms
        },
        complete: received == upload.declared_bytes,
    }
}

/// Checks a payload against its own header, returning the gaussians it declares.
fn probe_payload(kind: AssetKind, bytes: &[u8]) -> Result<Option<usize>, AssetError> {
    match kind {
        AssetKind::Ply => {
            let probe = ply_probe::probe(bytes).map_err(|reason| AssetError::Malformed { reason })?;
            if !probe.has_position {
                return Err(AssetError::Malformed {
                    reason: "the PLY vertex element has no x, y and z properties".to_owned(),
                });
            }
            Ok(Some(probe.points))
        }
        AssetKind::SplatBuffers => buffers::declared_count(bytes)
            .map(Some)
            .map_err(|reason| AssetError::Malformed { reason }),
        AssetKind::AttributePatch => Ok(None),
    }
}

/// The upload handle of a freshly staged upload.
impl AssetUpload {
    /// Stage an upload and describe it.
    pub fn begin(
        registry: &AssetRegistry,
        kind: AssetKind,
        declared_bytes: u64,
        declared_checksum: Option<u64>,
        provenance: impl Into<String>,
    ) -> Result<(Self, UploadStatus), AssetError> {
        let status = registry.begin_upload(kind, declared_bytes, declared_checksum, provenance)?;
        Ok((
            Self {
                upload_id: status.upload_id,
                kind: status.kind,
                declared_bytes: status.declared_bytes,
                declared_checksum,
            },
            status,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> AssetRegistry {
        AssetRegistry::with_session(0x4f2a, AssetBudgets::default())
    }

    fn ply_bytes(points: usize) -> Vec<u8> {
        let mut text = String::from("ply\nformat ascii 1.0\n");
        text.push_str(&format!("element vertex {points}\n"));
        for name in ["x", "y", "z", "f_dc_0", "f_dc_1", "f_dc_2", "opacity"] {
            text.push_str(&format!("property float {name}\n"));
        }
        text.push_str("end_header\n");
        text.into_bytes()
    }

    /// A PLY whose body is a whole number of vertices, so its byte length is predictable.
    fn binary_ply(points: usize) -> Vec<u8> {
        let mut text = String::from("ply\nformat binary_little_endian 1.0\n");
        text.push_str(&format!("element vertex {points}\n"));
        for name in ["x", "y", "z", "f_dc_0", "f_dc_1", "f_dc_2", "opacity"] {
            text.push_str(&format!("property float {name}\n"));
        }
        text.push_str("end_header\n");
        let mut bytes = text.into_bytes();
        bytes.extend(std::iter::repeat_n(0u8, points * 7 * 4));
        bytes
    }

    #[test]
    fn a_registered_file_is_snapshotted_once() {
        let directory = std::env::temp_dir().join(format!("splatmcp-asset-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("scene.ply");
        std::fs::write(&path, ply_bytes(3)).unwrap();

        let registry = registry();
        let info = registry.register_file(AssetKind::Ply, &path).unwrap();
        assert_eq!(info.point_count, Some(3));
        assert_eq!(info.bytes, ply_bytes(3).len());
        assert!(info.provenance.ends_with("scene.ply"));
        assert_eq!(info.schema, "ply");
        assert_eq!(info.contract_version, ASSET_CONTRACT_VERSION);
        assert!(info.is_live(info.created_at_ms));

        // The file changes afterwards: the snapshot does not.
        let handle = registry.resolve(&info.asset_id).unwrap();
        let snapshot = handle.bytes().to_vec();
        std::fs::write(&path, ply_bytes(9)).unwrap();
        let again = registry.resolve(&info.asset_id).unwrap();
        assert_eq!(again.bytes(), snapshot.as_slice());
        assert_eq!(again.checksum().bytes, ply_bytes(3).len());

        // A released id is not resolvable, whatever the file says now.
        assert!(registry.release(&info.asset_id).unwrap());
        assert_eq!(
            registry.resolve(&info.asset_id).unwrap_err().code(),
            "unknown_asset"
        );
        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn a_relative_path_and_a_missing_file_are_refused() {
        let registry = registry();
        let error = registry
            .register_file(AssetKind::Ply, Path::new("scene.ply"))
            .unwrap_err();
        assert_eq!(error.code(), "asset_relative_path");
        assert!(error.to_string().contains("absolute"));

        let error = registry
            .register_file(AssetKind::Ply, Path::new("C:/definitely/not/here.ply"))
            .unwrap_err();
        assert_eq!(error.code(), "asset_io");
    }

    #[test]
    fn a_malformed_or_truncated_payload_is_refused_at_submission() {
        let registry = registry();
        let error = registry
            .register_bytes(AssetKind::Ply, b"not a ply".to_vec(), "inline")
            .unwrap_err();
        assert_eq!(error.code(), "malformed_payload");

        // A binary payload that declares more gaussians than it carries is refused with the
        // numbers, not stored and repaired later.
        let mut truncated = binary_ply(4);
        truncated.truncate(truncated.len() - 8);
        let error = registry
            .register_bytes(AssetKind::Ply, truncated, "inline")
            .unwrap_err();
        assert_eq!(error.code(), "malformed_payload");
        assert!(error.to_string().contains("truncated"));

        let error = registry
            .register_with(
                AssetKind::Ply,
                ply_bytes(1),
                "inline".to_owned(),
                Some(0xdead_beef),
            )
            .unwrap_err();
        assert_eq!(error.code(), "checksum_mismatch");
    }

    #[test]
    fn budgets_bound_the_registry_and_eviction_is_least_recently_used() {
        use std::sync::atomic::{AtomicU64, Ordering};
        // A clock that advances by one per read, so "recently used" is exact rather than
        // decided by whichever entries happen to share a millisecond.
        let clock = Arc::new(AtomicU64::new(1_000));
        let reader = clock.clone();
        let registry = AssetRegistry::with_clock(
            1,
            AssetBudgets {
                max_assets: 2,
                ..AssetBudgets::default()
            },
            Arc::new(move || reader.fetch_add(1, Ordering::SeqCst)),
        );
        let first = registry
            .register_bytes(AssetKind::Ply, binary_ply(1), "first")
            .unwrap();
        let second = registry
            .register_bytes(AssetKind::Ply, binary_ply(2), "second")
            .unwrap();
        // Touch the first so the second becomes the least recently used.
        registry.resolve(&first.asset_id).unwrap();
        let third = registry
            .register_bytes(AssetKind::Ply, binary_ply(3), "third")
            .unwrap();

        assert!(registry.resolve(&first.asset_id).is_ok());
        assert!(registry.resolve(&third.asset_id).is_ok());
        assert_eq!(
            registry.resolve(&second.asset_id).unwrap_err().code(),
            "unknown_asset"
        );
        let stats = registry.stats().unwrap();
        assert_eq!(stats.assets, 2);
        assert_eq!(stats.evicted, 1);
        assert!(stats.bytes > 0, "the accounting reports what is held");
    }

    #[test]
    fn a_submission_above_the_declared_budget_is_refused_before_it_is_stored() {
        let registry = AssetRegistry::with_session(
            1,
            AssetBudgets {
                max_asset_bytes: 8,
                ..AssetBudgets::default()
            },
        );
        let error = registry
            .register_bytes(AssetKind::Ply, binary_ply(1), "too big")
            .unwrap_err();
        assert_eq!(error.code(), "budget_exceeded");
        assert!(error.to_string().contains("above the 8 limit"));
        assert_eq!(registry.stats().unwrap().assets, 0);
    }

    #[test]
    fn expiry_removes_the_id_but_a_live_handle_still_reads_its_bytes() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let clock = Arc::new(AtomicU64::new(1_000));
        let reader = clock.clone();
        let registry = AssetRegistry::with_clock(
            7,
            AssetBudgets {
                lifetime_ms: 100,
                ..AssetBudgets::default()
            },
            Arc::new(move || reader.load(Ordering::SeqCst)),
        );
        let info = registry
            .register_bytes(AssetKind::Ply, ply_bytes(1), "inline")
            .unwrap();
        assert_eq!(info.expires_at_ms, Some(1_100));
        let handle = registry.resolve(&info.asset_id).unwrap();

        clock.store(1_200, Ordering::SeqCst);
        assert_eq!(
            registry.resolve(&info.asset_id).unwrap_err().code(),
            "asset_expired"
        );
        assert!(!handle.bytes().is_empty(), "a held handle keeps its bytes");
        assert_eq!(registry.sweep().unwrap(), 0, "the expired id was already dropped");
        assert_eq!(registry.list().unwrap().len(), 0);
    }

    #[test]
    fn a_chunked_upload_is_resumable_and_finalized_atomically() {
        let registry = registry();
        let bytes = ply_bytes(5);
        let (_, status) = AssetUpload::begin(
            &registry,
            AssetKind::Ply,
            bytes.len() as u64,
            Some(checksum_of(&bytes).value),
            "chunked",
        )
        .unwrap();
        assert!(!status.complete);
        assert_eq!(status.next_offset, 0);

        // A chunk that would leave a hole is refused with the offset to use.
        let error = registry.upload_append(status.upload_id, 4, &bytes[..4]).unwrap_err();
        assert_eq!(error.code(), "upload_offset_mismatch");
        assert!(error.to_string().contains("next offset is 0"));

        let half = bytes.len() / 2;
        let progress = registry
            .upload_append(status.upload_id, 0, &bytes[..half])
            .unwrap();
        assert_eq!(progress.received_bytes as usize, half);
        assert_eq!(progress.next_offset as usize, half);
        // Finalizing early is refused: nothing is registered.
        let error = registry.upload_finalize(status.upload_id).unwrap_err();
        assert_eq!(error.code(), "truncated_payload");

        registry
            .upload_append(status.upload_id, half as u64, &bytes[half..])
            .unwrap();
        let info = registry.upload_finalize(status.upload_id).unwrap();
        assert_eq!(info.point_count, Some(5));
        assert_eq!(registry.resolve(&info.asset_id).unwrap().bytes(), bytes.as_slice());
        assert_eq!(
            registry.upload_status(status.upload_id).unwrap_err().code(),
            "unknown_upload"
        );

        // A wrong checksum fails at finalize, not after a partial registration.
        let (_, other) = AssetUpload::begin(
            &registry,
            AssetKind::Ply,
            bytes.len() as u64,
            Some(0),
            "chunked",
        )
        .unwrap();
        registry.upload_append(other.upload_id, 0, &bytes).unwrap();
        assert_eq!(
            registry.upload_finalize(other.upload_id).unwrap_err().code(),
            "checksum_mismatch"
        );
        assert_eq!(registry.stats().unwrap().assets, 1);

        // An oversized chunk is refused, and an abandoned upload is dropped by a sweep.
        let (_, abandoned) = AssetUpload::begin(&registry, AssetKind::Ply, 8, None, "staged").unwrap();
        assert!(registry.upload_cancel(abandoned.upload_id).unwrap());
        assert!(!registry.upload_cancel(abandoned.upload_id).unwrap());
    }
}
