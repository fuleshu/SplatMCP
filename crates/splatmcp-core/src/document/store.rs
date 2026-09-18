//! The authoritative document store: identity, revisions, snapshots and retention.
//!
//! The store owns one *displayed* document plus a bounded window of previously replaced
//! documents and revisions. Everything a caller can observe is either the active document or
//! an exact [`DocumentHandle`], and content is held behind `Arc` so a snapshot is an
//! immutable view that survives later edits without copying anything.
//!
//! Locking rules that the rest of the app depends on:
//!
//! - The store's mutex protects *identity, provenance and the `Arc` pointers*, never a long
//!   computation. Parsing, serialising and inspecting happen on an `Arc<Splat>` the caller
//!   already holds, outside the lock.
//! - A mutation's expectation is checked inside the same critical section as the swap, so a
//!   stale candidate gets an explicit conflict rather than overwriting newer work.
//! - Copy-on-write means a retained snapshot never changes: `Arc::make_mut` clones only when
//!   somebody is still reading that revision.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::document::metadata::{DocumentMetadata, ExportRecord, RevisionRecord, trim_history};
use crate::document::{
    DocumentError, DocumentHandle, DocumentId, Expected, MAX_EXPORTS, Mutation, Provenance,
};
use crate::{Bounds, Splat};

/// Milliseconds since the Unix epoch, saturating at the epoch.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default()
}

/// How much history the store keeps for handles that arrive late.
///
/// Retention is what makes a snapshot handle a *time-bounded* promise: the newest revision of
/// the displayed document is always resolvable, older revisions and replaced documents are
/// kept until either bound is reached, and anything evicted fails with
/// [`DocumentError::SnapshotExpired`] rather than resolving to something else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionLimits {
    /// Revisions kept across every document, including the active one.
    pub max_revisions: usize,
    /// Bytes of gaussian data kept across every document.
    pub max_bytes: usize,
}

impl RetentionLimits {
    pub const fn new(max_revisions: usize, max_bytes: usize) -> Self {
        Self {
            max_revisions,
            max_bytes,
        }
    }
}

impl Default for RetentionLimits {
    /// Eight revisions, or half a gigabyte of gaussian data, whichever comes first.
    ///
    /// Enough for a viewer fetch, a concurrent job and one undo step, small enough that a
    /// series of edits on a large scene cannot accumulate copies of it.
    fn default() -> Self {
        Self {
            max_revisions: 8,
            max_bytes: 512 * 1024 * 1024,
        }
    }
}

/// What retention currently costs, for metadata and diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionStats {
    /// Documents still addressable, including the active one.
    pub documents: usize,
    /// Revisions still resolvable.
    pub revisions: usize,
    /// Pins currently held.
    pub pins: usize,
    /// Bytes of gaussian data retained.
    pub bytes: usize,
    pub max_revisions: usize,
    pub max_bytes: usize,
}

impl RetentionStats {
    /// True when held pins are keeping more than the configured budget alive.
    pub fn over_budget(&self) -> bool {
        self.revisions > self.max_revisions || self.bytes > self.max_bytes
    }
}

/// An immutable view of one exact revision.
///
/// The `Arc` inside keeps the content alive for as long as the caller holds the snapshot, so a
/// slow operation cannot be torn by a later edit.
#[derive(Debug, Clone)]
pub struct Snapshot {
    handle: DocumentHandle,
    splat: Arc<Splat>,
    provenance: Provenance,
    metadata: DocumentMetadata,
}

impl Snapshot {
    fn new(
        handle: DocumentHandle,
        splat: Arc<Splat>,
        provenance: Provenance,
        metadata: DocumentMetadata,
    ) -> Self {
        Self {
            handle,
            splat,
            provenance,
            metadata,
        }
    }

    /// Exact identity of this snapshot.
    pub fn handle(&self) -> &DocumentHandle {
        &self.handle
    }

    /// The content, shared rather than copied.
    pub fn splat(&self) -> &Arc<Splat> {
        &self.splat
    }

    /// Provenance as it was when this revision was produced.
    pub fn provenance(&self) -> &Provenance {
        &self.provenance
    }

    /// Bounded metadata of this revision.
    pub fn metadata(&self) -> &DocumentMetadata {
        &self.metadata
    }

    pub fn len(&self) -> usize {
        self.splat.len()
    }

    pub fn is_empty(&self) -> bool {
        self.splat.is_empty()
    }
}

/// A pin: retention keeps this revision resolvable until the pin is released.
///
/// Jobs and captures read an owned copy and need no pin; a pin exists for a handle that will
/// be resolved *later* by identity - an export that runs after the edit that triggered it, an
/// undo step, or a request that fetched metadata and comes back for the bytes.
///
/// A pin is identified by the token minted for it, and [`DocumentStore::release`] consumes
/// that token exactly once. Releasing an already released pin reports `false` and changes
/// nothing, so a double release cannot drop protection another reader still holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotPin {
    id: u64,
    handle: DocumentHandle,
}

impl SnapshotPin {
    /// Opaque pin id, for logs.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Revision the pin keeps alive.
    pub fn handle(&self) -> &DocumentHandle {
        &self.handle
    }

    /// Mint the same token again, as a caller would if it copied the pin.
    ///
    /// Used by tests to prove that a repeated token cannot release somebody else's pin.
    pub fn duplicate(&self) -> Self {
        self.clone()
    }
}

/// A pin that releases itself when it goes out of scope.
///
/// The reason this exists: a fallible path that pins a revision and then returns early on an
/// error would otherwise leak the pin, and leaked pins keep whole scenes alive past the
/// retention budget. Dropping the guard releases exactly one token, on every exit path.
pub struct PinGuard<'a> {
    store: &'a DocumentStore,
    pin: SnapshotPin,
}

impl PinGuard<'_> {
    /// Revision the guard keeps alive.
    pub fn handle(&self) -> &DocumentHandle {
        self.pin.handle()
    }

    /// The underlying pin, for callers that need to release it early.
    pub fn pin(&self) -> &SnapshotPin {
        &self.pin
    }

    /// Releases the pin now. Dropping the guard afterwards is harmless.
    pub fn release(mut self) -> bool {
        let released = self.store.release(&self.pin);
        // Mark the token as spent so the Drop below cannot release it a second time.
        self.pin.id = SPENT_TOKEN;
        released
    }
}

impl std::fmt::Debug for PinGuard<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PinGuard")
            .field("pin", &self.pin)
            .finish()
    }
}

impl Drop for PinGuard<'_> {
    fn drop(&mut self) {
        if self.pin.id != SPENT_TOKEN {
            self.store.release(&self.pin);
        }
    }
}

/// Token value that can never be minted, used to mark a released guard.
const SPENT_TOKEN: u64 = u64::MAX;

/// One retained revision that is no longer the newest of its document.
///
/// The count and bounds are cached here for the same reason the newest revision caches them:
/// describing a revision must not walk its gaussians, and must never happen under the store's
/// lock.
#[derive(Debug, Clone)]
struct RetainedRevision {
    revision: u64,
    splat: Arc<Splat>,
    point_count: usize,
    bounds: Option<Bounds>,
    provenance: Provenance,
    /// Tokens of the pins keeping this revision alive.
    pins: HashSet<u64>,
    serial: u64,
}

/// One document: its newest revision plus the recent revisions before it.
#[derive(Debug, Clone)]
struct Entry {
    id: DocumentId,
    revision: u64,
    splat: Arc<Splat>,
    point_count: usize,
    bounds: Option<Bounds>,
    provenance: Provenance,
    /// Accepted changes, oldest first, bounded by [`crate::document::MAX_HISTORY`].
    history: Vec<RevisionRecord>,
    /// Previous revisions, newest first.
    older: VecDeque<RetainedRevision>,
    /// Tokens of the pins on the newest revision.
    pins: HashSet<u64>,
    serial: u64,
}

impl Entry {
    /// Total gaussian bytes this document retains.
    fn bytes(&self) -> usize {
        bytes_of(&self.splat)
            + self
                .older
                .iter()
                .map(|revision| bytes_of(&revision.splat))
                .sum::<usize>()
    }

    fn handle(&self) -> DocumentHandle {
        DocumentHandle::new(self.id.clone(), self.revision)
    }

    /// Revisions of this document that can still be resolved, newest first.
    fn retained_revisions(&self) -> Vec<u64> {
        let mut revisions = Vec::with_capacity(self.older.len() + 1);
        revisions.push(self.revision);
        revisions.extend(self.older.iter().map(|revision| revision.revision));
        revisions
    }
}

/// Bytes one revision's gaussian data occupies.
fn bytes_of(splat: &Splat) -> usize {
    splat.points.len() * std::mem::size_of::<crate::SplatPoint>()
}

/// Mutable state behind the store's mutex.
#[derive(Debug, Default)]
struct Inner {
    entries: Vec<Entry>,
    active: Option<DocumentId>,
    /// Identities minted this session, so an evicted handle is reported as expired rather
    /// than as something that never existed.
    known: VecDeque<DocumentId>,
    /// Order in which revisions were created, used to evict the oldest first.
    serial: u64,
    mutations: u64,
}

impl Inner {
    fn position(&self, id: &DocumentId) -> Option<usize> {
        self.entries.iter().position(|entry| &entry.id == id)
    }

    fn active_entry(&self) -> Option<&Entry> {
        self.active
            .as_ref()
            .and_then(|id| self.position(id))
            .map(|index| &self.entries[index])
    }

    fn active_handle(&self) -> Option<DocumentHandle> {
        self.active_entry().map(Entry::handle)
    }

    /// Bounded statistics of what retention currently holds.
    fn stats(&self, limits: RetentionLimits) -> RetentionStats {
        RetentionStats {
            documents: self.entries.len(),
            revisions: self.entries.iter().map(|entry| entry.older.len() + 1).sum(),
            pins: self
                .entries
                .iter()
                .map(|entry| {
                    entry.pins.len() + entry.older.iter().map(|older| older.pins.len()).sum::<usize>()
                })
                .sum(),
            bytes: self.entries.iter().map(Entry::bytes).sum(),
            max_revisions: limits.max_revisions,
            max_bytes: limits.max_bytes,
        }
    }

    /// Oldest evictable unit: never the newest revision of the active document, never a
    /// pinned revision.
    ///
    /// Returns the entry index plus the index of a retained revision, or `None` for that
    /// second slot when the whole entry (an inactive document's newest revision) can go.
    fn oldest_evictable(&self) -> Option<(usize, Option<usize>)> {
        let mut best: Option<(u64, usize, Option<usize>)> = None;
        for (index, entry) in self.entries.iter().enumerate() {
            let is_active_newest = self.active.as_ref().is_some_and(|id| id == &entry.id);
            if !is_active_newest && entry.pins.is_empty() {
                let candidate = (entry.serial, index, None);
                if best.is_none_or(|current| candidate.0 < current.0) {
                    best = Some(candidate);
                }
            }
            for (older_index, older) in entry.older.iter().enumerate() {
                if older.pins.is_empty() {
                    let candidate = (older.serial, index, Some(older_index));
                    if best.is_none_or(|current| candidate.0 < current.0) {
                        best = Some(candidate);
                    }
                }
            }
        }
        best.map(|(_, index, older)| (index, older))
    }

    /// Drops evictable history until both bounds are satisfied.
    ///
    /// Pins win: retention never drops a pinned revision, so a caller holding one can always
    /// resolve it, and [`RetentionStats::over_budget`] reports the temporary overshoot.
    fn evict(&mut self, limits: RetentionLimits) {
        while {
            let stats = self.stats(limits);
            stats.over_budget()
        } {
            let Some((index, older)) = self.oldest_evictable() else {
                return;
            };
            match older {
                Some(older_index) => {
                    self.entries[index].older.remove(older_index);
                }
                None => {
                    // The identity stays in `known` on purpose: an evicted handle is
                    // reported as expired, not as something that never existed.
                    self.entries.remove(index);
                }
            }
        }
    }

    /// Records an identity, bounded, so eviction stays distinguishable from "never existed".
    fn remember(&mut self, id: &DocumentId) {
        if self.known.len() >= 64 {
            self.known.pop_front();
        }
        self.known.push_back(id.clone());
    }
}

/// The store: one active document plus bounded, addressable history.
pub struct DocumentStore {
    inner: Mutex<Inner>,
    session: u64,
    limits: RetentionLimits,
    minted: AtomicU64,
    pins: AtomicU64,
}

impl Default for DocumentStore {
    fn default() -> Self {
        Self::new(RetentionLimits::default())
    }
}

impl DocumentStore {
    /// A store with the given retention and a session stamp of its own.
    pub fn new(limits: RetentionLimits) -> Self {
        Self::with_session(limits, next_session())
    }

    /// A store with an explicit session stamp, for tests and for a restored project.
    ///
    /// Restoring a project deliberately hands back a stamp, which is the only way a handle
    /// from an earlier run can keep meaning something: everything else starts a fresh session
    /// whose identities cannot collide with the previous one.
    pub fn with_session(limits: RetentionLimits, session: u64) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            session,
            limits,
            minted: AtomicU64::new(1),
            pins: AtomicU64::new(1),
        }
    }

    /// Session stamp of the identities this store mints.
    pub fn session_id(&self) -> u64 {
        self.session
    }

    pub fn limits(&self) -> RetentionLimits {
        self.limits
    }

    /// What retention currently holds.
    pub fn stats(&self) -> RetentionStats {
        self.lock().stats(self.limits)
    }

    /// Identity of the displayed document, when one is loaded.
    pub fn active_handle(&self) -> Option<DocumentHandle> {
        self.lock().active_handle()
    }

    /// Bounded metadata of the displayed document.
    pub fn active_metadata(&self) -> Option<DocumentMetadata> {
        let inner = self.lock();
        inner.active_entry().map(metadata_of)
    }

    /// Bounded metadata of an exact revision, if it is still retained.
    pub fn metadata_for(&self, handle: &DocumentHandle) -> Result<DocumentMetadata, DocumentError> {
        Ok(self.resolve_entry(handle)?.1)
    }

    /// Makes something else the displayed document: a **new identity**, at revision 1.
    ///
    /// Opening a file is the case this exists for. Two opens of the same path are two
    /// documents, because a path is provenance and not identity.
    pub fn open(&self, splat: Splat, mutation: Mutation) -> DocumentMetadata {
        let at_ms = mutation.at_ms.unwrap_or_else(now_ms);
        let point_count = splat.len();
        let bounds = splat.bounds();
        let mut inner = self.lock();
        let id = self.mint();
        inner.remember(&id);
        inner.serial += 1;
        let serial = inner.serial;
        inner.mutations += 1;
        let entry = Entry {
            id: id.clone(),
            revision: 1,
            splat: Arc::new(splat),
            point_count,
            bounds,
            provenance: Provenance::new(&mutation, at_ms),
            history: vec![RevisionRecord::new(
                1,
                mutation.kind,
                mutation.operation.clone(),
                at_ms,
            )],
            older: VecDeque::new(),
            pins: HashSet::new(),
            serial,
        };
        let metadata = metadata_of(&entry);
        inner.entries.push(entry);
        inner.active = Some(id);
        inner.evict(self.limits);
        metadata
    }

    /// Applies a content change under a compare-and-swap check.
    ///
    /// [`Expected::Any`] is the "whatever is displayed" case: it resolves to the active
    /// document at request receipt, or starts a new document when nothing is displayed, and
    /// the resolved handle comes back in the returned metadata.
    pub fn commit(
        &self,
        expected: Expected,
        splat: Splat,
        mutation: Mutation,
    ) -> Result<DocumentMetadata, DocumentError> {
        if !mutation.kind.advances_revision() {
            return Err(DocumentError::NoDocument);
        }
        // Both are properties of the incoming content, so they are computed before the lock.
        let point_count = splat.len();
        let bounds = splat.bounds();
        let at_ms = mutation.at_ms.unwrap_or_else(now_ms);
        let mut inner = self.lock();

        let index = match &expected {
            Expected::Any => match inner.active.clone() {
                Some(id) => inner.position(&id).expect("the active document is present"),
                None => {
                    drop(inner);
                    return Ok(self.open(splat, mutation));
                }
            },
            Expected::Revision(revision) => {
                let Some(entry) = inner.active_entry() else {
                    return Err(DocumentError::NoDocument);
                };
                let current = entry.handle();
                if current.revision != *revision {
                    let expected = DocumentHandle::new(current.document_id.clone(), *revision);
                    return Err(DocumentError::Conflict { expected, current });
                }
                inner
                    .position(&current.document_id)
                    .expect("the active document is present")
            }
            Expected::Handle(handle) => {
                let active = inner.active.clone();
                if active.as_ref() != Some(&handle.document_id) {
                    // A named document that is not the displayed one never resolves to
                    // whatever happens to be displayed: that is the silent-retarget bug.
                    return Err(DocumentError::UnknownDocument {
                        document_id: handle.document_id.clone(),
                        active,
                    });
                }
                let position = inner
                    .position(&handle.document_id)
                    .expect("the active document is present");
                let entry = &inner.entries[position];
                if entry.revision != handle.revision {
                    return Err(DocumentError::Conflict {
                        expected: handle.clone(),
                        current: entry.handle(),
                    });
                }
                position
            }
        };

        inner.serial += 1;
        let serial = inner.serial;
        inner.mutations += 1;
        let entry = &mut inner.entries[index];
        // Copy-on-write: a retained snapshot of the previous revision keeps pointing at the
        // old content, and nothing is copied unless somebody is still reading it.
        // Outstanding tokens travel with the revision they protect; the new revision starts
        // with none, because nothing has read it yet.
        let carried = std::mem::take(&mut entry.pins);
        entry.older.push_front(RetainedRevision {
            revision: entry.revision,
            splat: Arc::clone(&entry.splat),
            point_count: entry.point_count,
            bounds: entry.bounds,
            provenance: entry.provenance.clone(),
            pins: carried,
            serial: entry.serial,
        });
        entry.revision += 1;
        entry.splat = Arc::new(splat);
        entry.point_count = point_count;
        entry.bounds = bounds;
        entry.serial = serial;
        entry.provenance.apply(&mutation, at_ms);
        entry.history.push(RevisionRecord::new(
            entry.revision,
            mutation.kind,
            mutation.operation.clone(),
            at_ms,
        ));
        trim_history(&mut entry.history);
        let metadata = metadata_of(entry);
        inner.evict(self.limits);
        Ok(metadata)
    }

    /// Changes only the named component of the displayed document.
    ///
    /// Component metadata is part of what a revision describes, so this advances the revision
    /// too, even though no gaussian moved; the geometry itself is shared, not copied.
    pub fn set_component(
        &self,
        expected: Expected,
        component_id: impl Into<String>,
        operation: impl Into<String>,
    ) -> Result<DocumentMetadata, DocumentError> {
        let at_ms = now_ms();
        let mut inner = self.lock();
        let handle = match &expected {
            Expected::Any => inner.active_handle().ok_or(DocumentError::NoDocument)?,
            Expected::Revision(revision) => {
                let entry = inner.active_entry().ok_or(DocumentError::NoDocument)?;
                let current = entry.handle();
                if current.revision != *revision {
                    return Err(DocumentError::Conflict {
                        expected: DocumentHandle::new(current.document_id.clone(), *revision),
                        current,
                    });
                }
                current
            }
            Expected::Handle(handle) => {
                let current = inner.active_handle();
                if current.as_ref() != Some(handle) {
                    return Err(match current {
                        Some(current) => DocumentError::Conflict {
                            expected: handle.clone(),
                            current,
                        },
                        None => DocumentError::NoDocument,
                    });
                }
                handle.clone()
            }
        };
        let index = inner
            .position(&handle.document_id)
            .expect("the active document is present");
        let mutation = Mutation::component_change(component_id, operation).at_ms(at_ms);
        inner.serial += 1;
        let serial = inner.serial;
        inner.mutations += 1;
        let entry = &mut inner.entries[index];
        let carried = std::mem::take(&mut entry.pins);
        entry.older.push_front(RetainedRevision {
            revision: entry.revision,
            splat: Arc::clone(&entry.splat),
            point_count: entry.point_count,
            bounds: entry.bounds,
            provenance: entry.provenance.clone(),
            pins: carried,
            serial: entry.serial,
        });
        entry.revision += 1;
        entry.serial = serial;
        entry.provenance.apply(&mutation, at_ms);
        entry.history.push(RevisionRecord::new(
            entry.revision,
            mutation.kind,
            mutation.operation.clone(),
            at_ms,
        ));
        trim_history(&mut entry.history);
        let metadata = metadata_of(entry);
        inner.evict(self.limits);
        Ok(metadata)
    }

    /// Records that `handle`'s revision was written to `path`.
    ///
    /// Exporting changes neither geometry nor component metadata, so the revision does **not**
    /// advance; the artifact checksum identifies those exact bytes, while the document keeps
    /// the identity it already had.
    pub fn record_export(
        &self,
        handle: &DocumentHandle,
        path: impl Into<String>,
        checksum: crate::document::ArtifactChecksum,
        at_ms: u64,
    ) -> Result<DocumentMetadata, DocumentError> {
        let mut inner = self.lock();
        let (index, revision) = match inner.position(&handle.document_id) {
            Some(index) => {
                let entry = &inner.entries[index];
                let revision = if entry.revision == handle.revision {
                    entry.revision
                } else if entry
                    .older
                    .iter()
                    .any(|older| older.revision == handle.revision)
                {
                    handle.revision
                } else {
                    return Err(DocumentError::SnapshotExpired {
                        handle: handle.clone(),
                    });
                };
                (index, revision)
            }
            None => {
                return Err(DocumentError::UnknownDocument {
                    document_id: handle.document_id.clone(),
                    active: inner.active.clone(),
                });
            }
        };
        let entry = &mut inner.entries[index];
        entry.provenance.exports.insert(
            0,
            ExportRecord {
                path: path.into(),
                revision,
                at_ms,
                checksum,
            },
        );
        entry.provenance.exports.truncate(MAX_EXPORTS);
        Ok(metadata_of(entry))
    }

    /// An immutable snapshot of what a request demands.
    ///
    /// The returned snapshot owns its `Arc`, so the lock is released before the caller does
    /// anything with the content.
    pub fn snapshot(&self, expected: Expected) -> Result<Snapshot, DocumentError> {
        if let Expected::Handle(handle) = &expected {
            // Resolved without the lock held, because a handle can live in a document that is
            // no longer displayed.
            let handle = handle.clone();
            let (_, metadata, splat) = self.resolve_entry(&handle)?;
            return Ok(snapshot_of(metadata, splat));
        }
        let inner = self.lock();
        let entry = match &expected {
            Expected::Any => inner.active_entry().ok_or(DocumentError::NoDocument)?,
            Expected::Revision(revision) => {
                let entry = inner.active_entry().ok_or(DocumentError::NoDocument)?;
                if entry.revision != *revision {
                    return Err(DocumentError::Conflict {
                        expected: DocumentHandle::new(entry.id.clone(), *revision),
                        current: entry.handle(),
                    });
                }
                entry
            }
            Expected::Handle(_) => unreachable!("handled above"),
        };
        let splat = Arc::clone(&entry.splat);
        Ok(snapshot_of(metadata_of(entry), splat))
    }

    /// Resolves an exact handle, wherever that revision now lives.
    ///
    /// A retained document that is no longer displayed still resolves, which is what lets a
    /// slow operation on document A finish after document B was opened.
    pub fn resolve(&self, handle: &DocumentHandle) -> Result<Snapshot, DocumentError> {
        self.snapshot(Expected::Handle(handle.clone()))
    }

    /// Pins a revision so retention keeps it resolvable.
    ///
    /// Every call mints its own token, so the caller's protection is independent of every
    /// other reader's.
    pub fn pin(&self, handle: &DocumentHandle) -> Result<SnapshotPin, DocumentError> {
        let mut inner = self.lock();
        let id = self.pins.fetch_add(1, Ordering::SeqCst);
        let position = inner.position(&handle.document_id).ok_or_else(|| {
            if inner.known.contains(&handle.document_id) {
                DocumentError::SnapshotExpired {
                    handle: handle.clone(),
                }
            } else {
                DocumentError::UnknownDocument {
                    document_id: handle.document_id.clone(),
                    active: inner.active.clone(),
                }
            }
        })?;
        let entry = &mut inner.entries[position];
        if entry.revision == handle.revision {
            entry.pins.insert(id);
        } else if let Some(older) = entry
            .older
            .iter_mut()
            .find(|older| older.revision == handle.revision)
        {
            older.pins.insert(id);
        } else {
            return Err(DocumentError::SnapshotExpired {
                handle: handle.clone(),
            });
        }
        Ok(SnapshotPin {
            id,
            handle: handle.clone(),
        })
    }

    /// Pins a revision and returns a guard that releases it on the way out.
    ///
    /// Use this when the pin covers a fallible sequence - serialising, writing a file,
    /// recording an export - so an early return cannot leak it.
    pub fn pin_guarded(
        &self,
        handle: &DocumentHandle,
    ) -> Result<PinGuard<'_>, DocumentError> {
        Ok(PinGuard {
            store: self,
            pin: self.pin(handle)?,
        })
    }

    /// Releases one pin, identified by the token it was minted with.
    ///
    /// Returns false when that token had already been released: the token is consumed exactly
    /// once, so releasing a pin twice can never drop another reader's protection.
    pub fn release(&self, pin: &SnapshotPin) -> bool {
        let mut inner = self.lock();
        let Some(position) = inner.position(&pin.handle.document_id) else {
            return false;
        };
        let entry = &mut inner.entries[position];
        if entry.revision == pin.handle.revision {
            return entry.pins.remove(&pin.id);
        }
        match entry
            .older
            .iter_mut()
            .find(|older| older.revision == pin.handle.revision)
        {
            Some(older) => older.pins.remove(&pin.id),
            None => false,
        }
    }

    /// Resolves a handle to an entry's metadata and content.
    fn resolve_entry(
        &self,
        handle: &DocumentHandle,
    ) -> Result<(usize, DocumentMetadata, Arc<Splat>), DocumentError> {
        let inner = self.lock();
        let Some(position) = inner.position(&handle.document_id) else {
            return Err(if inner.known.contains(&handle.document_id) {
                DocumentError::SnapshotExpired {
                    handle: handle.clone(),
                }
            } else {
                DocumentError::UnknownDocument {
                    document_id: handle.document_id.clone(),
                    active: inner.active.clone(),
                }
            });
        };
        let entry = &inner.entries[position];
        if entry.revision == handle.revision {
            let metadata = metadata_of(entry);
            return Ok((position, metadata, Arc::clone(&entry.splat)));
        }
        match entry
            .older
            .iter()
            .find(|older| older.revision == handle.revision)
        {
            Some(older) => {
                let metadata = metadata_with_content(entry, older);
                Ok((position, metadata, Arc::clone(&older.splat)))
            }
            None => Err(DocumentError::SnapshotExpired {
                handle: handle.clone(),
            }),
        }
    }

    /// Mints the next identity of this session.
    fn mint(&self) -> DocumentId {
        DocumentId::mint(self.session, self.minted.fetch_add(1, Ordering::SeqCst))
    }

    /// Locks the store, recovering from a poisoned lock.
    ///
    /// The state behind it is plain data with no invariant a panic can break, so recovering is
    /// strictly better than turning one thread's panic into a dead app.
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Pairs bounded metadata with the content it describes.
fn snapshot_of(metadata: DocumentMetadata, splat: Arc<Splat>) -> Snapshot {
    Snapshot::new(
        metadata.handle.clone(),
        splat,
        metadata.provenance.clone(),
        metadata,
    )
}

/// Bounded metadata of one entry's newest revision.
fn metadata_of(entry: &Entry) -> DocumentMetadata {
    DocumentMetadata {
        handle: entry.handle(),
        point_count: entry.point_count,
        bounds: entry.bounds,
        attributes: crate::document::metadata::attributes(),
        provenance: entry.provenance.clone(),
        history: entry.history.iter().rev().cloned().collect(),
        retained_revisions: entry.retained_revisions(),
    }
}

/// Metadata of a retained revision: its own provenance and revision, the document's history.
fn metadata_with_content(entry: &Entry, older: &RetainedRevision) -> DocumentMetadata {
    DocumentMetadata {
        handle: DocumentHandle::new(entry.id.clone(), older.revision),
        point_count: older.point_count,
        bounds: older.bounds,
        attributes: crate::document::metadata::attributes(),
        provenance: older.provenance.clone(),
        history: entry.history.iter().rev().cloned().collect(),
        retained_revisions: entry.retained_revisions(),
    }
}

/// Session stamp for a store: the process start time combined with a per-process counter.
///
/// The stamp is what makes a handle from an earlier run invalid rather than accidentally
/// matching a freshly minted document.
fn next_session() -> u64 {
    static START: OnceLock<u64> = OnceLock::new();
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let start = *START.get_or_init(|| now_ms() & 0xffff_ffff);
    (start << 16) | (NEXT.fetch_add(1, Ordering::SeqCst) & 0xffff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SplatPoint;
    use crate::document::Mutation;

    fn splat(points: usize) -> Splat {
        Splat::from_points(
            (0..points)
                .map(|index| {
                    SplatPoint::new(
                        [index as f32, 0.0, 0.0],
                        [0.1; 3],
                        [0.5; 3],
                        0.8,
                        [1.0, 0.0, 0.0, 0.0],
                    )
                })
                .collect(),
        )
    }

    fn store(limits: RetentionLimits) -> DocumentStore {
        DocumentStore::with_session(limits, 1)
    }

    #[test]
    fn a_session_is_stamped_so_two_runs_cannot_share_an_identity() {
        let first = DocumentStore::with_session(RetentionLimits::default(), 1);
        let second = DocumentStore::with_session(RetentionLimits::default(), 2);
        let a = first.open(splat(1), Mutation::import("a.ply"));
        let b = second.open(splat(1), Mutation::import("a.ply"));
        assert_ne!(a.handle.document_id, b.handle.document_id);
        // A fresh store reports a foreign handle as unknown, not as a conflict.
        let error = second
            .resolve(&a.handle)
            .expect_err("another session's handle must not resolve");
        assert_eq!(error.code(), "unknown_document");
    }

    #[test]
    fn retention_evicts_the_oldest_but_keeps_a_pinned_revision() {
        let store = store(RetentionLimits::new(3, usize::MAX));
        let first = store.open(splat(2), Mutation::import("first.ply"));
        let pinned = store.pin(&first.handle).unwrap();
        let second = store
            .commit(Expected::Any, splat(3), Mutation::edit("edit"))
            .unwrap();
        let third = store
            .commit(Expected::Any, splat(4), Mutation::edit("edit"))
            .unwrap();
        let fourth = store
            .commit(Expected::Any, splat(5), Mutation::edit("edit"))
            .unwrap();

        // Three revisions fit; the pinned oldest one survives and revision 2 is dropped.
        let stats = store.stats();
        assert_eq!(stats.revisions, 3, "{stats:?}");
        assert_eq!(stats.pins, 1);
        assert!(!stats.over_budget(), "{stats:?}");
        assert_eq!(
            store.resolve(&first.handle).unwrap().len(),
            2,
            "the pin held it"
        );
        assert_eq!(
            store.resolve(&second.handle).unwrap_err().code(),
            "snapshot_expired"
        );
        assert_eq!(third.handle.revision, 3);
        assert_eq!(fourth.handle.revision, 4);

        // Releasing the pin lets retention catch up on the next change.
        assert!(store.release(&pinned));
        let after = store
            .commit(Expected::Any, splat(6), Mutation::edit("edit"))
            .unwrap();
        assert_eq!(after.handle.revision, 5);
        assert_eq!(store.stats().pins, 0);
        assert_eq!(
            store.resolve(&first.handle).unwrap_err().code(),
            "snapshot_expired"
        );
        assert!(store.stats().revisions <= 3);
    }

    #[test]
    fn two_pins_on_one_revision_are_independent_tokens() {
        let store = store(RetentionLimits::new(1, usize::MAX));
        let first = store.open(splat(6), Mutation::import("first.ply"));
        let a = store.pin(&first.handle).unwrap();
        let b = store.pin(&first.handle).unwrap();
        assert_eq!(store.stats().pins, 2);

        // Releasing one pin leaves the other reader protected.
        assert!(store.release(&a));
        assert_eq!(store.stats().pins, 1, "one reader is still holding the revision");

        // Releasing the same token again changes nothing: it was consumed above.
        assert!(!store.release(&a), "a released token cannot release again");
        assert_eq!(store.stats().pins, 1);

        // A copied token cannot double-release either.
        let copy = b.duplicate();
        assert!(store.release(&b));
        assert!(!store.release(&copy), "a copied token is the same token");
        assert_eq!(store.stats().pins, 0);

        // With every token consumed retention can catch up again.
        store
            .commit(Expected::Any, splat(1), Mutation::edit("edit"))
            .unwrap();
        store
            .commit(Expected::Any, splat(1), Mutation::edit("edit"))
            .unwrap();
        assert_eq!(
            store.resolve(&first.handle).unwrap_err().code(),
            "snapshot_expired"
        );
    }

    #[test]
    fn a_pin_guard_releases_on_every_exit_path() {
        let store = store(RetentionLimits::new(1, usize::MAX));
        let opened = store.open(splat(4), Mutation::import("scene.ply"));

        // The early return stands for a fallible serializer or a failed write.
        fn fallible(store: &DocumentStore, handle: &DocumentHandle) -> Result<(), &'static str> {
            let guard = store
                .pin_guarded(handle)
                .map_err(|_| "the revision is not retained")?;
            assert_eq!(guard.handle().revision, 1);
            assert_eq!(store.stats().pins, 1, "the guard holds one pin");
            Err("the write failed")
        }

        assert_eq!(fallible(&store, &opened.handle), Err("the write failed"));
        assert_eq!(store.stats().pins, 0, "the guard released the pin on the way out");

        // A guard that is released explicitly does not release a second time on drop.
        {
            let guard = store.pin_guarded(&opened.handle).unwrap();
            assert!(guard.release());
        }
        assert_eq!(store.stats().pins, 0);
        // The revision is still the displayed one, so it is still resolvable.
        assert_eq!(store.resolve(&opened.handle).unwrap().len(), 4);
    }

    #[test]
    fn a_pin_holds_retention_above_its_budget_rather_than_breaking_a_promise() {
        let store = store(RetentionLimits::new(1, usize::MAX));
        let first = store.open(splat(4), Mutation::import("first.ply"));
        let pinned = store.pin(&first.handle).unwrap();
        store
            .commit(Expected::Any, splat(1), Mutation::edit("edit"))
            .unwrap();
        store
            .commit(Expected::Any, splat(1), Mutation::edit("edit"))
            .unwrap();

        let stats = store.stats();
        assert!(stats.over_budget(), "{stats:?}");
        assert_eq!(stats.pins, 1);
        assert_eq!(store.resolve(&first.handle).unwrap().len(), 4);

        assert!(store.release(&pinned));
        store
            .commit(Expected::Any, splat(1), Mutation::edit("edit"))
            .unwrap();
        assert!(!store.stats().over_budget(), "{:?}", store.stats());
    }

    #[test]
    fn evicted_and_foreign_handles_are_told_apart() {
        let store = store(RetentionLimits::new(1, usize::MAX));
        let first = store.open(splat(1), Mutation::import("a.ply"));
        store
            .commit(Expected::Any, splat(1), Mutation::edit("edit"))
            .unwrap();
        let foreign = DocumentHandle::new(DocumentId::mint(9, 1), 1);
        assert_eq!(
            store.resolve(&foreign).unwrap_err().code(),
            "unknown_document"
        );
        // A single-revision window evicts document A entirely once B is opened.
        store.open(splat(1), Mutation::import("b.ply"));
        assert_eq!(
            store.resolve(&first.handle).unwrap_err().code(),
            "snapshot_expired"
        );
    }
}
