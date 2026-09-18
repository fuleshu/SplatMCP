//! Compact metadata and provenance for a document revision.
//!
//! Everything here is bounded: a fixed number of scalars, one bounds box, a short history and
//! a few export records. Describing a 500 000 gaussian document therefore costs the same as
//! describing three, and never involves serialising geometry or holding the document lock.

use crate::contract::ATTRIBUTES;
use crate::document::{MAX_HISTORY, MutationKind, Provenance};
use crate::{Bounds, DocumentHandle};

/// One accepted change to a document, as recorded in its history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionRecord {
    /// Revision the change produced.
    pub revision: u64,
    /// What kind of change it was.
    pub kind: MutationKind,
    /// Operation or job that produced it, when the caller named one.
    pub operation: Option<String>,
    pub at_ms: u64,
}

impl RevisionRecord {
    pub fn new(revision: u64, kind: MutationKind, operation: Option<String>, at_ms: u64) -> Self {
        Self {
            revision,
            kind,
            operation,
            at_ms,
        }
    }
}

/// Identity of one *encoded artifact*: a file this app wrote.
///
/// Deliberately separate from the document's content identity (`document_id` + `revision`).
/// The checksum identifies those exact bytes - the PLY encoding, its property order and its
/// writer version - so it answers "is this the file I wrote?", not "is this the same scene?".
/// Two exports of the same revision can differ; a document that round trips through a file
/// keeps its identity though the bytes differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtifactChecksum {
    /// Algorithm name, so a consumer never guesses what the value means.
    pub algorithm: &'static str,
    /// 64-bit FNV-1a of the encoded bytes. Not a cryptographic digest.
    pub value: u64,
    /// Size of the encoded artifact, in bytes.
    pub bytes: usize,
}

impl ArtifactChecksum {
    /// Algorithm used for every artifact this app reports.
    pub const ALGORITHM: &'static str = "fnv1a64";

    /// Checksum of `bytes`.
    pub fn of(bytes: &[u8]) -> Self {
        Self {
            algorithm: Self::ALGORITHM,
            value: fingerprint(bytes),
            bytes: bytes.len(),
        }
    }

    /// Lowercase hexadecimal form, for replies and logs.
    pub fn hex(&self) -> String {
        format!("{:016x}", self.value)
    }

    /// True when the checksum names the same bytes as `other`.
    pub fn matches(&self, other: &Self) -> bool {
        self.algorithm == other.algorithm && self.value == other.value && self.bytes == other.bytes
    }
}

/// FNV-1a 64 over `bytes`.
///
/// Chosen because it needs no dependency and is stable across runs and platforms, which is
/// what makes it usable as a *recorded* artifact identity in replies and sidecars.
pub fn fingerprint(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// One recorded export: where a revision was written, and which bytes went there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportRecord {
    /// Destination the caller chose.
    pub path: String,
    /// Revision that was exported.
    pub revision: u64,
    pub at_ms: u64,
    pub checksum: ArtifactChecksum,
}

/// Bounded description of one document revision.
#[derive(Debug, Clone, PartialEq)]
pub struct DocumentMetadata {
    /// Exact identity this metadata describes.
    pub handle: DocumentHandle,
    pub point_count: usize,
    /// Bounds padded by each gaussian's largest radius.
    pub bounds: Option<Bounds>,
    /// Attributes the model stores, from the Gaussian contract.
    pub attributes: &'static [&'static str],
    pub provenance: Provenance,
    /// Accepted changes, newest first, at most [`MAX_HISTORY`].
    pub history: Vec<RevisionRecord>,
    /// Revisions of this document that can still be resolved, newest first.
    pub retained_revisions: Vec<u64>,
}

impl DocumentMetadata {
    /// Identity of the content, as text: what a caller quotes back.
    pub fn identity(&self) -> String {
        self.handle.to_string()
    }

    /// Most recent export, when there is one.
    pub fn last_export(&self) -> Option<&ExportRecord> {
        self.provenance.exports.first()
    }

    /// One line, bounded, for a reply or a log.
    pub fn summary(&self) -> String {
        let bounds = self
            .bounds
            .map(|bounds| format!("radius {:.3}", bounds.radius))
            .unwrap_or_else(|| "no bounds".to_owned());
        format!(
            "{} revision {} of {} gaussians ({bounds}), updated {} ms ago",
            self.handle.document_id,
            self.handle.revision,
            self.point_count,
            crate::document::now_ms().saturating_sub(self.provenance.updated_at_ms)
        )
    }
}

impl std::fmt::Display for DocumentMetadata {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.summary())
    }
}

/// Attributes every document revision carries, from the contract.
pub fn attributes() -> &'static [&'static str] {
    &ATTRIBUTES
}

/// Keeps only the newest [`MAX_HISTORY`] records, dropping the oldest first.
pub(crate) fn trim_history(history: &mut Vec<RevisionRecord>) {
    if history.len() > MAX_HISTORY {
        let excess = history.len() - MAX_HISTORY;
        history.drain(..excess);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::DocumentId;

    #[test]
    fn a_checksum_names_the_bytes_not_the_scene() {
        let first = ArtifactChecksum::of(b"ply bytes");
        let second = ArtifactChecksum::of(b"ply bytes");
        assert!(first.matches(&second));
        assert_eq!(first.hex().len(), 16);
        assert_eq!(first.bytes, 9);
        assert_eq!(first.algorithm, "fnv1a64");

        // The same scene written by a different encoder is a different artifact.
        assert!(!first.matches(&ArtifactChecksum::of(b"ply bytes ")));
        assert_ne!(first.value, ArtifactChecksum::of(b"other").value);
    }

    #[test]
    fn fingerprint_is_stable_and_sensitive() {
        assert_eq!(fingerprint(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fingerprint(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_ne!(fingerprint(b"ab"), fingerprint(b"ba"));
    }

    #[test]
    fn metadata_is_a_bounded_description() {
        let handle = DocumentHandle::new(DocumentId::mint(2, 1), 4);
        let metadata = DocumentMetadata {
            handle: handle.clone(),
            point_count: 500_000,
            bounds: Some(Bounds {
                min: [-1.0; 3],
                max: [1.0; 3],
                center: [0.0; 3],
                radius: 1.0,
            }),
            attributes: attributes(),
            provenance: Provenance::new(&crate::document::Mutation::import("scene.ply"), 0),
            history: Vec::new(),
            retained_revisions: vec![4, 3, 2, 1],
        };
        assert_eq!(metadata.identity(), "doc-2-1@4");
        assert_eq!(metadata.attributes.len(), 5);
        assert!(metadata.last_export().is_none());
        assert!(metadata.summary().len() < 200, "{}", metadata.summary());
    }

    #[test]
    fn history_is_trimmed_to_its_bound() {
        let mut history: Vec<RevisionRecord> = (1..=(MAX_HISTORY as u64 + 5))
            .map(|revision| RevisionRecord::new(revision, MutationKind::Edit, None, revision))
            .collect();
        trim_history(&mut history);
        assert_eq!(history.len(), MAX_HISTORY);
        assert_eq!(history.last().unwrap().revision, MAX_HISTORY as u64 + 5);
        assert_eq!(history.first().unwrap().revision, 6);
    }
}
